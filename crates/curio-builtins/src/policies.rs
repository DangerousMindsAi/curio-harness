//! Built-in hook plugins: system instructions, retry on transient errors, and tool approval.

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstructionsCode {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
}

/// The instructions plugin's state slot: whether its prompt is in the log already.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InstructionsState {
    pub applied: bool,
}

#[async_trait]
impl Runner for InstructionsRunner {
    fn name(&self) -> &str {
        "builtin.instructions"
    }

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
pub struct Instructions {
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

    fn decode_state(&self, blob: Value) -> Result<Box<dyn PluginState>> {
        decode_state::<InstructionsState>(blob)
    }
}

/// `builtin.retry`: retries transient model errors with exponential backoff.
pub struct RetryRunner;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryCode {
    max_retries: u32,
    #[serde(default = "default_base_delay")]
    base_delay_ms: u64,
    #[serde(default = "default_max_delay")]
    max_delay_ms: u64,
}

fn default_base_delay() -> u64 {
    1000
}
fn default_max_delay() -> u64 {
    30_000
}

#[async_trait]
impl Runner for RetryRunner {
    fn name(&self) -> &str {
        "builtin.retry"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        Ok(LoadedPlugin::stateless(Arc::new(Retry {
            code: env.parse_code(code)?,
        })))
    }
}

/// Retries transient failures. The attempt count comes from the log, so it needs no state.
pub struct Retry {
    code: RetryCode,
}

#[async_trait]
impl Plugin for Retry {
    fn subscriptions(&self) -> Vec<Subscription> {
        vec![Subscription::new(HookName::RequestError)]
    }

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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalCode {
    /// Tools that need an answer from the user before each call.
    #[serde(default)]
    ask: Vec<String>,
    /// Tools that are always refused.
    #[serde(default)]
    deny: Vec<String>,
}

#[async_trait]
impl Runner for ApprovalRunner {
    fn name(&self) -> &str {
        "builtin.approval"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        Ok(LoadedPlugin::stateless(Arc::new(Approval {
            code: env.parse_code(code)?,
        })))
    }
}

/// Decides each listed call in `tool_pre`.
pub struct Approval {
    code: ApprovalCode,
}

#[async_trait]
impl Plugin for Approval {
    fn subscriptions(&self) -> Vec<Subscription> {
        vec![Subscription::new(HookName::ToolPre)]
    }

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
