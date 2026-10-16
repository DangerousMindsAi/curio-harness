//! Agent and runtime configuration. Both are plain data, read from YAML by the host and
//! recorded as JSON in an agent's log header and image.
//!
//! There are two levels. A [`RuntimeConfig`] describes one process: its host id, which
//! runners agents may use, and the three runtime-level plugins (log store, provisioner and
//! secret store) that [`crate::Runtime::start`] loads before any agent exists. An
//! [`AgentConfig`] describes one agent: its id, tenant, crash-resume policy and every
//! plugin it runs, each as a [`PluginSpec`] naming a runner, the code that runner loads and
//! the [`PluginGrants`] that bound what the plugin may reach.
//!
//! An agent's config comes from the provisioner once, at creation. The runtime writes it
//! into the log header and every image carries a copy, so a thawed or recovered agent
//! loads exactly the plugins it had before and never asks the provisioner again. The
//! structs deny unknown fields so a typo fails at load instead of being silently ignored.

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
    /// The agent's id; also the key its log is stored under.
    pub id: AgentId,
    /// The tenant whose secrets the agent's plugins resolve.
    pub tenant: TenantId,
    /// What happens to a turn that a crash interrupted. Required, so each deployment decides.
    pub resume: ResumeConfig,
    /// Every plugin, in hook order (ties in a hook's `order` are broken by this position).
    /// Must include exactly one driver and one message queue; see [`crate::PluginTable::build`].
    pub plugins: Vec<PluginSpec>,
}

/// Whether an agent continues a crashed turn by itself. Read by
/// [`crate::repair_after_crash`] after the interrupted turn has been closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "after_crash", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResumeConfig {
    /// Close the interrupted turn and wait for the next message.
    Wait,
    /// Close the interrupted turn and start a new one with a notice, at most `max_resumes`
    /// times in a row. The count resets when a turn completes.
    Continue { max_resumes: u32 },
}

/// One plugin: the runner that loads it, the code the runner interprets, and the grants
/// that bound what it can reach.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    /// The plugin's id within this agent. Unique per agent; names its state slot in images.
    pub id: PluginId,
    /// Runner name, such as `builtin.tool.shell`.
    pub runner: String,
    /// What the runner loads. For built-in runners this is a configuration table. Absent
    /// code is `null`, which [`crate::LoadEnv::parse_code`] treats as an empty table.
    #[serde(default)]
    pub code: Value,
    /// What the plugin may touch. Defaults to nothing at all, except that any hook is
    /// allowed while `hooks` is absent.
    #[serde(default)]
    pub grants: PluginGrants,
}

/// What one plugin may touch. Everything not listed is denied.
///
/// The core enforces `hooks` (in [`crate::PluginTable::build`]) and `secrets` (in
/// [`crate::SecretScope::resolve`]). `fs` and `hosts` are handed to the runner and the tool
/// body, and each runner enforces them for the operations it offers; for example,
/// [`crate::LoadEnv::require_host`] checks a host at load time.
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
///
/// It has no id field: the runtime names the three plugins `log_store`, `provisioner` and
/// `secret_store` itself, and loads them for the reserved tenant `host`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePluginSpec {
    /// Runner name. Looked up in the registry directly; the `runners` allow-list applies
    /// only to agent plugins.
    pub runner: String,
    /// What the runner loads, as in [`PluginSpec::code`].
    #[serde(default)]
    pub code: Value,
    /// What the plugin may touch. The secret store's own scope has no store behind it, so
    /// it can't resolve secrets.
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
    /// Runner names agents may use. An entry ending in `*` enables every runner whose name
    /// starts with the part before the `*`, so `builtin.*` enables all built-in runners.
    /// Every entry must match at least one registered runner, or start fails.
    pub runners: Vec<String>,
    /// Where every agent's session log is kept.
    pub log_store: RuntimePluginSpec,
    /// Produces the configuration of each new agent.
    pub provisioner: RuntimePluginSpec,
    /// Resolves secret names to values for each tenant.
    pub secret_store: RuntimePluginSpec,
}
