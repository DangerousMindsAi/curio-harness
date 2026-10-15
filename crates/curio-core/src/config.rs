//! Agent and runtime configuration. Both are plain data, read from YAML by the host and
//! recorded as JSON in an agent's log header and image.

use crate::hook::HookName;
use crate::ids::{AgentId, PluginId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// The full configuration of one agent. Pinned in the agent's log header at creation and
/// carried in every image, so a thawed or recovered agent behaves as it did before.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: AgentId,
    pub tenant: TenantId,
    /// What happens to a turn that a crash interrupted. Required, so each deployment decides.
    pub resume: ResumeConfig,
    /// Every plugin, in hook order (ties in a hook's `order` are broken by this position).
    pub plugins: Vec<PluginSpec>,
}

/// Whether an agent continues a crashed turn by itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "after_crash", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResumeConfig {
    /// Close the interrupted turn and wait for the next message.
    Wait,
    /// Close the interrupted turn and start a new one with a notice, at most `max_resumes`
    /// times in a row.
    Continue { max_resumes: u32 },
}

/// One plugin: the runner that loads it, the code the runner interprets, and the grants
/// that bound what it can reach.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    pub id: PluginId,
    /// Runner name, such as `builtin.tool.shell`.
    pub runner: String,
    /// What the runner loads. For built-in runners this is a configuration table.
    #[serde(default)]
    pub code: Value,
    #[serde(default)]
    pub grants: PluginGrants,
}

/// What one plugin may touch. Everything not listed is denied.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGrants {
    /// Filesystem roots. Tools that touch files stay inside the first root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs: Vec<PathBuf>,
    /// Network hosts the plugin may connect to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Secret names, resolved per use within the agent's tenant. Values never enter logs or images.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Hooks the plugin may subscribe to. When absent, any hook is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<HookName>>,
}

/// A runtime-level plugin: configured once per process in the host's startup file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePluginSpec {
    pub runner: String,
    #[serde(default)]
    pub code: Value,
    #[serde(default)]
    pub grants: PluginGrants,
}

/// The process-level configuration the runtime starts from: which runners agents may
/// use, and the three runtime-level plugins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Names this host in `Thawed` and `Recovered` log events.
    pub host_id: String,
    /// Runner names agents may use. An entry ending in `.*` enables every runner with that prefix.
    pub runners: Vec<String>,
    /// Where every agent's session log is kept.
    pub log_store: RuntimePluginSpec,
    /// Produces the configuration of each new agent.
    pub provisioner: RuntimePluginSpec,
    /// Resolves secret names to values for each tenant.
    pub secret_store: RuntimePluginSpec,
}
