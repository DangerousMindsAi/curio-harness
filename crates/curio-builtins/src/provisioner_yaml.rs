//! `builtin.provisioner.yaml-dir`: creates agents from YAML files in a directory. The
//! template name is the file name without `.yaml`.

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

/// Loads [`YamlDirProvisioner`].
pub struct YamlDirProvisionerRunner;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionerCode {
    dir: PathBuf,
}

#[async_trait]
impl Runner for YamlDirProvisionerRunner {
    fn name(&self) -> &str {
        "builtin.provisioner.yaml-dir"
    }

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

/// Reads agent configurations from `<dir>/<template>.yaml`.
pub struct YamlDirProvisioner {
    dir: PathBuf,
}

impl Plugin for YamlDirProvisioner {
    fn as_provisioner(&self) -> Option<&dyn Provisioner> {
        Some(self)
    }
}

/// Parses an agent configuration from YAML.
pub fn parse_agent_yaml(text: &str) -> Result<AgentConfig> {
    serde_yaml_ng::from_str(text).map_err(Error::config)
}

#[async_trait]
impl Provisioner for YamlDirProvisioner {
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
