//! Built-in runners for curio-harness. Each runner's code is a configuration table.
//!
//! | Runner name | Provides |
//! |---|---|
//! | `builtin.driver.react` | the default loop |
//! | `builtin.model.openai-compat` | any OpenAI-compatible chat completions API, such as DeepInfra |
//! | `builtin.model.scripted` | fixed replies, for tests and offline demos |
//! | `builtin.queue.file` | a message queue in local files |
//! | `builtin.logstore.file` | session logs in local files (runtime-level) |
//! | `builtin.provisioner.yaml-dir` | agents from YAML files (runtime-level) |
//! | `builtin.secrets.inline` | secrets from the host's startup file (runtime-level) |
//! | `builtin.instructions` | a system prompt |
//! | `builtin.retry` | retries of transient model errors |
//! | `builtin.approval` | asking before, or refusing, tool calls |
//! | `builtin.tool.clock`, `builtin.tool.shell`, `builtin.tool.read_file`, `builtin.tool.write_file` | tools |

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

/// A registry with every built-in runner.
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
