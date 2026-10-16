//! Built-in runners for curio-harness.
//!
//! Every behavior in curio-harness is a plugin: a runner (Rust code compiled into the host
//! and registered under a runner name) plus the code that runner interprets. For the
//! runners in this crate the code is a configuration table, written as YAML in an agent
//! file or in the host's startup file and handed to [`curio_core::Runner::load`] as JSON.
//! Each runner validates its table at load, so a typo fails when the agent is created,
//! thawed or recovered instead of halfway through a turn.
//!
//! Most runners are per-agent: an agent file lists them under `plugins`, and each agent gets
//! its own immutable plugin instance. Three are runtime-level: the host's startup file names
//! one log store, one provisioner and one secret store for the whole process, and
//! [`curio_core::Runtime::start`] loads them before any agent exists.
//!
//! | Runner name | Module | Level | Provides |
//! |---|---|---|---|
//! | `builtin.driver.react` | [`driver`] | agent | the default loop |
//! | `builtin.model.openai-compat` | [`model_openai`] | agent | any OpenAI-compatible chat completions API, such as DeepInfra |
//! | `builtin.model.scripted` | [`model_scripted`] | agent | fixed replies, for tests and offline demos |
//! | `builtin.queue.file` | [`queue_file`] | agent | a message queue in local files |
//! | `builtin.logstore.file` | [`logstore_file`] | runtime | session logs in local files |
//! | `builtin.provisioner.yaml-dir` | [`provisioner_yaml`] | runtime | agents from YAML files |
//! | `builtin.secrets.inline` | [`secrets_inline`] | runtime | secrets from the host's startup file |
//! | `builtin.instructions` | [`policies`] | agent | a system prompt |
//! | `builtin.retry` | [`policies`] | agent | retries of transient model errors |
//! | `builtin.approval` | [`policies`] | agent | asking before, or refusing, tool calls |
//! | `builtin.tool.clock`, `builtin.tool.shell`, `builtin.tool.read_file`, `builtin.tool.write_file` | [`tools`] | agent | tools |
//!
//! An agent needs exactly one driver and one message queue (checked by
//! [`curio_core::PluginTable::build`]), at least one model provider for the driver to name,
//! and whatever hook plugins and tools it wants. A minimal agent file looks like this:
//!
//! ```yaml
//! id: helper
//! tenant: local
//! resume: { after_crash: wait }
//! plugins:
//!   - id: driver
//!     runner: builtin.driver.react
//!     code: { model: model, max_steps_per_turn: 20, max_forced_continuations: 2 }
//!   - id: model
//!     runner: builtin.model.scripted
//!     code: { replies: [{ text: "Hello." }] }
//!   - id: queue
//!     runner: builtin.queue.file
//!     code: { dir: data/queues }
//! ```
//!
//! [`registry`] registers all of these at once; a host that wants fewer can build its own
//! [`RunnerRegistry`] and also limit what agents may use with the startup file's `runners`
//! list.

pub mod driver;
mod fsutil;
pub mod logstore_file;
pub mod model_openai;
pub mod model_scripted;
pub mod policies;
pub mod provisioner_yaml;
pub mod queue_file;
pub mod secrets_inline;
pub mod tools;

use curio_core::{Result, RunnerRegistry};

/// A registry with every built-in runner, under the names in the table above.
///
/// Fails only if two runners share a name, which would be a bug in this crate. The file
/// queue runner keeps one broker per queue directory, so agents loaded through the same
/// registry that name the same `dir` share wakeups and the one-consumer check.
pub fn registry() -> Result<RunnerRegistry> {
    let mut r = RunnerRegistry::new();
    r.register(driver::ReactDriverRunner)?;
    r.register(model_openai::OpenAiCompatRunner::new())?;
    r.register(model_scripted::ScriptedModelRunner)?;
    r.register(queue_file::FileQueueRunner::default())?;
    r.register(logstore_file::FileLogStoreRunner)?;
    r.register(provisioner_yaml::YamlDirProvisionerRunner)?;
    r.register(secrets_inline::InlineSecretsRunner)?;
    r.register(policies::InstructionsRunner)?;
    r.register(policies::RetryRunner)?;
    r.register(policies::ApprovalRunner)?;
    r.register(tools::ClockRunner)?;
    r.register(tools::ShellRunner)?;
    r.register(tools::FileToolRunner::read())?;
    r.register(tools::FileToolRunner::write())?;
    Ok(r)
}
