//! Built-in hook plugins: system instructions, retry on transient errors, and tool approval.
//!
//! None of these provide a service. Each is a per-agent plugin that subscribes to a few
//! hooks the driver dispatches and changes the loop's behavior only through the hook
//! payloads and the log. They're small on purpose: they show the three ways a policy can
//! act on the loop.
//!
//! `builtin.instructions` adds model-visible input. On `agent_loaded` it appends a
//! `SystemPrompt` event once per agent, and keeps a state slot recording that it did, so
//! thawing or recovering the agent doesn't add the prompt a second time. On `replay` it
//! rebuilds that flag from the log.
//!
//! ```yaml
//! - id: instructions
//!   runner: builtin.instructions
//!   code: { text: "Be brief." }       # or { file: prompts/assistant.md }
//! ```
//!
//! `builtin.retry` answers `request_error`. For transient model errors it sets the
//! decision to retry after an exponential backoff, and the driver does the waiting. It
//! needs no state: the attempt count is `failed_attempts_this_step` in the view, which is
//! folded from the `AttemptFailed` events the driver logs.
//!
//! ```yaml
//! - id: retry
//!   runner: builtin.retry
//!   code: { max_retries: 4, base_delay_ms: 1000, max_delay_ms: 30000 }
//! ```
//!
//! `builtin.approval` answers `tool_pre`. Calls to tools in `deny` are refused with a
//! model-visible reason; calls to tools in `ask` become a question to whoever holds the
//! agent's handle, and the driver waits for the answer.
//!
//! ```yaml
//! - id: approval
//!   runner: builtin.approval
//!   code: { ask: [shell], deny: [write_file] }
//! ```
//!
//! Listeners run in config order within a hook (all three subscribe with order 0), so put
//! `approval` before any other `tool_pre` plugin whose decision it should see or override.

use async_trait::async_trait;
use curio_core::{
    Error, ErrorDecision, Event, Flow, HookCx, HookEvent, HookName, LoadEnv, LoadedPlugin,
    ModelErrorKind, Plugin, PluginState, Result, Runner, Subscription, ToolDecision, decode_state,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `builtin.instructions`: contributes a system prompt, from inline text or a file read at load.
pub struct InstructionsRunner;

/// The instructions plugin's code. Exactly one of the two fields must be set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstructionsCode {
    /// The prompt text itself.
    #[serde(default)]
    text: Option<String>,
    /// A file holding the prompt, relative to the startup file's directory. Read once at
    /// load; editing the file later changes nothing for an agent whose prompt is already
    /// logged.
    #[serde(default)]
    file: Option<PathBuf>,
}

/// The instructions plugin's state slot: whether its prompt is in the log already.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InstructionsState {
    /// Set once this plugin's `SystemPrompt` event has been appended (live) or seen
    /// (during replay). Carried in images, so a thawed agent doesn't log the prompt again.
    pub applied: bool,
}

#[async_trait]
impl Runner for InstructionsRunner {
    fn name(&self) -> &str {
        "builtin.instructions"
    }

    /// Reads the prompt from `text` or `file`. Fails when both or neither are set, or when
    /// the file can't be read. Starts with an empty slot (`applied: false`).
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: InstructionsCode = env.parse_code(code)?;
        let text = match (code.text, code.file) {
            (Some(text), None) => text,
            (None, Some(file)) => tokio::fs::read_to_string(env.resolve_path(file)).await?,
            _ => {
                return Err(Error::plugin(
                    &env.plugin,
                    "set exactly one of `text` and `file`",
                ));
            }
        };
        Ok(LoadedPlugin {
            instance: Arc::new(Instructions { text }),
            initial_state: Box::new(InstructionsState::default()),
        })
    }
}

/// Logs its system prompt once, when the agent is first loaded.
///
/// The prompt becomes part of the history through the log, so a recovered agent sees it
/// without this plugin doing anything. Because the event is tagged with this plugin's id,
/// a second `SystemPrompt` from the same plugin would replace the first in the history.
pub struct Instructions {
    /// The prompt, as read at load.
    text: String,
}

#[async_trait]
impl Plugin for Instructions {
    fn subscriptions(&self) -> Vec<Subscription> {
        vec![
            Subscription::new(HookName::AgentLoaded),
            Subscription::new(HookName::Replay),
        ]
    }

    /// On `agent_loaded`, appends the prompt unless the slot says it's already logged. On
    /// `replay`, marks the slot when it sees its own `SystemPrompt` event. Always continues.
    async fn on_hook(&self, cx: &mut HookCx<'_>, event: &mut HookEvent<'_>) -> Result<Flow> {
        match event {
            HookEvent::AgentLoaded { .. } => {
                if !cx.state::<InstructionsState>()?.applied {
                    let source = cx.plugin.clone();
                    cx.append(Event::SystemPrompt {
                        source,
                        text: self.text.clone(),
                    })?;
                    cx.state::<InstructionsState>()?.applied = true;
                }
            }
            HookEvent::Replay {
                event: Event::SystemPrompt { source, .. },
            } if source == cx.plugin => {
                cx.state::<InstructionsState>()?.applied = true;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Restores [`InstructionsState`] from an image.
    fn decode_state(&self, blob: Value) -> Result<Box<dyn PluginState>> {
        decode_state::<InstructionsState>(blob)
    }
}

/// `builtin.retry`: retries transient model errors with exponential backoff.
pub struct RetryRunner;

/// The retry plugin's code.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryCode {
    /// How many retries one step may make after its first failed attempt. Required; 0
    /// turns retrying off.
    max_retries: u32,
    /// The delay before the first retry, in milliseconds. Each later retry doubles it.
    /// Defaults to 1000.
    #[serde(default = "default_base_delay")]
    base_delay_ms: u64,
    /// The longest delay, in milliseconds. Defaults to 30000.
    #[serde(default = "default_max_delay")]
    max_delay_ms: u64,
}

/// Default for [`RetryCode::base_delay_ms`].
fn default_base_delay() -> u64 {
    1000
}
/// Default for [`RetryCode::max_delay_ms`].
fn default_max_delay() -> u64 {
    30_000
}

#[async_trait]
impl Runner for RetryRunner {
    fn name(&self) -> &str {
        "builtin.retry"
    }

    /// Parses the code; there's nothing else to check.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        Ok(LoadedPlugin::stateless(Arc::new(Retry {
            code: env.parse_code(code)?,
        })))
    }
}

/// Retries transient failures. The attempt count comes from the log, so it needs no state.
///
/// Only [`ModelErrorKind::Transient`] errors are retried. A context overflow is left to
/// fail, since sending the same history again can't succeed; a compaction plugin would be
/// the one to prove progress and ask for a retry.
pub struct Retry {
    /// The validated code.
    code: RetryCode,
}

#[async_trait]
impl Plugin for Retry {
    fn subscriptions(&self) -> Vec<Subscription> {
        vec![Subscription::new(HookName::RequestError)]
    }

    /// Sets the decision to retry when the error is transient and the step still has
    /// retries left, leaving it untouched otherwise. Always continues, so a later listener
    /// can still override the decision.
    ///
    /// The driver logs `AttemptFailed` before dispatching, so the count here already
    /// includes this failure: the first retry sees 1 and waits `base_delay_ms`, the second
    /// waits twice that, and so on, capped at `max_delay_ms` (the doubling stops at 2^16).
    async fn on_hook(&self, cx: &mut HookCx<'_>, event: &mut HookEvent<'_>) -> Result<Flow> {
        if let HookEvent::RequestError { error, decision } = event {
            let failed = cx.view().counters.failed_attempts_this_step;
            if error.kind == ModelErrorKind::Transient && failed <= self.code.max_retries {
                let delay = self
                    .code
                    .base_delay_ms
                    .saturating_mul(1 << (failed.saturating_sub(1)).min(16));
                **decision = ErrorDecision::Retry {
                    after: Duration::from_millis(delay.min(self.code.max_delay_ms)),
                };
            }
        }
        Ok(Flow::Continue)
    }
}

/// `builtin.approval`: asks before, or refuses, calls to listed tools.
pub struct ApprovalRunner;

/// The approval plugin's code. Tool names aren't checked against the agent's tools, so a
/// misspelled name is silently never matched.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalCode {
    /// Tools that need an answer from the user before each call. Defaults to none.
    #[serde(default)]
    ask: Vec<String>,
    /// Tools that are always refused. Defaults to none. A tool in both lists is refused.
    #[serde(default)]
    deny: Vec<String>,
}

#[async_trait]
impl Runner for ApprovalRunner {
    fn name(&self) -> &str {
        "builtin.approval"
    }

    /// Parses the code; there's nothing else to check.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        Ok(LoadedPlugin::stateless(Arc::new(Approval {
            code: env.parse_code(code)?,
        })))
    }
}

/// Decides each listed call in `tool_pre`. Calls to unlisted tools pass through untouched.
pub struct Approval {
    /// The validated code.
    code: ApprovalCode,
}

#[async_trait]
impl Plugin for Approval {
    fn subscriptions(&self) -> Vec<Subscription> {
        vec![Subscription::new(HookName::ToolPre)]
    }

    /// Denies a call to a `deny` tool and stops the dispatch, so no later listener can
    /// turn the denial back into an allow. Turns an allowed call to an `ask` tool into a
    /// question that shows the tool name and its raw arguments; a call an earlier listener
    /// already denied or asked about is left as it is.
    async fn on_hook(&self, _cx: &mut HookCx<'_>, event: &mut HookEvent<'_>) -> Result<Flow> {
        if let HookEvent::ToolPre { call, decision } = event {
            if self.code.deny.contains(&call.name) {
                **decision = ToolDecision::Deny {
                    reason: format!("The `{}` tool is not allowed here.", call.name),
                };
                return Ok(Flow::Stop);
            }
            if self.code.ask.contains(&call.name) && **decision == ToolDecision::Allow {
                **decision = ToolDecision::Ask {
                    question: format!("Run {} with {}?", call.name, call.arguments),
                };
            }
        }
        Ok(Flow::Continue)
    }
}
