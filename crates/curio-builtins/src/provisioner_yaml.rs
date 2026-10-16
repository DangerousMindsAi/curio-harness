//! `builtin.provisioner.yaml-dir`: creates agents from YAML files in a directory.
//!
//! This is a runtime-level plugin. The host's startup file names it as the `provisioner`,
//! and it implements [`Provisioner`] with no hooks and no state. [`curio_core::Runtime::create`]
//! calls it once per new agent; after that the agent's configuration lives in its log
//! header and its images, so thaw and recovery never ask the provisioner again, and editing
//! a template file changes only agents created afterwards.
//!
//! A [`CreateRequest`]'s template name picks the file `<dir>/<template>.yaml`, which holds
//! one [`AgentConfig`] (the same format as `examples/agents/assistant.yaml`). When the
//! request carries an agent id, it replaces the file's `id`, which is how one template
//! backs several agents. The request's `params` are ignored, and the spec it returns has no
//! initial state or events.
//!
//! ```yaml
//! provisioner:
//!   runner: builtin.provisioner.yaml-dir
//!   code: { dir: examples/agents }   # relative to the startup file's directory
//! ```

use crate::fsutil::path_component;
use async_trait::async_trait;
use curio_core::{
    AgentConfig, AgentSpec, CreateRequest, Error, LoadEnv, LoadedPlugin, Plugin, Provisioner,
    Result, Runner,
};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Loads [`YamlDirProvisioner`]. Holds nothing itself.
pub struct YamlDirProvisionerRunner;

/// The provisioner's code.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionerCode {
    /// The template directory. Required; relative paths resolve against the startup file's
    /// directory.
    dir: PathBuf,
}

#[async_trait]
impl Runner for YamlDirProvisionerRunner {
    fn name(&self) -> &str {
        "builtin.provisioner.yaml-dir"
    }

    /// Fails unless `dir` exists and is a directory. Templates aren't read or checked
    /// until an agent is created from them.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: ProvisionerCode = env.parse_code(code)?;
        let dir = env.resolve_path(&code.dir);
        if !tokio::fs::metadata(&dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Err(Error::plugin(
                &env.plugin,
                format!("{} is not a directory", dir.display()),
            ));
        }
        Ok(LoadedPlugin::stateless(Arc::new(YamlDirProvisioner {
            dir,
        })))
    }
}

/// Reads agent configurations from `<dir>/<template>.yaml`, fresh on every request.
pub struct YamlDirProvisioner {
    /// The resolved template directory.
    dir: PathBuf,
}

impl Plugin for YamlDirProvisioner {
    fn as_provisioner(&self) -> Option<&dyn Provisioner> {
        Some(self)
    }
}

/// Parses an agent configuration from YAML.
///
/// [`AgentConfig`] denies unknown fields, so a misspelled key fails here with serde's
/// message. It's public so a host can parse agent files the same way. It checks only the
/// format: runner names, code tables and grants are checked later, when the
/// runtime loads each plugin.
pub fn parse_agent_yaml(text: &str) -> Result<AgentConfig> {
    serde_yaml_ng::from_str(text).map_err(Error::config)
}

#[async_trait]
impl Provisioner for YamlDirProvisioner {
    /// Reads and parses the template, applying the requested agent id if there is one.
    ///
    /// Fails with a configuration error when the template name isn't a single path
    /// component, the file can't be read, or it doesn't parse. The file's own `id` is
    /// still required even when the request overrides it.
    async fn provision(&self, request: CreateRequest) -> Result<AgentSpec> {
        let path = self
            .dir
            .join(format!("{}.yaml", path_component(&request.template)?));
        let text = tokio::fs::read_to_string(&path).await.map_err(|e| {
            Error::config(format!(
                "cannot read agent template {}: {e}",
                path.display()
            ))
        })?;
        let mut config = parse_agent_yaml(&text)
            .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        if let Some(id) = request.agent_id {
            config.id = id;
        }
        Ok(AgentSpec {
            config,
            initial_state: Vec::new(),
            initial_events: Vec::new(),
        })
    }
}
