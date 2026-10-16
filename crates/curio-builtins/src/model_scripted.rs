//! `builtin.model.scripted`: a model provider that plays back a fixed list of replies,
//! for tests and offline demos.
//!
//! It's a per-agent plugin that implements [`ModelProvider`] and subscribes to no hooks.
//! The driver names it in its `model` field like any other provider.
//!
//! The reply it gives is chosen by how many assistant messages the request already holds:
//! the first request of the agent's life gets reply 1, the request after that reply gets
//! reply 2, and so on across every turn. Because that count comes from the history, which
//! is a projection of the log, the plugin needs no state and picks the same reply after a
//! thaw or a crash recovery. A failed attempt adds no assistant message, so a retry gets
//! the same reply again. Once the script runs out, every request fails with a fatal error,
//! which ends the turn with [`curio_core::TurnEnd::Error`].
//!
//! ```yaml
//! - id: model
//!   runner: builtin.model.scripted
//!   code:
//!     delay_ms: 300                       # optional; gives cancels and drains a window
//!     replies:
//!       - text: "Checking."
//!         tool_calls: [{ name: clock }]   # arguments default to {}
//!       - text: "It is now."
//! ```
//!
//! Call ids are `call-<reply>-<call>`, both counted from 0, so they're unique within one
//! agent's script and stable across restarts.

use async_trait::async_trait;
use curio_core::{
    AssistantMessage, CallId, Finish, LoadEnv, LoadedPlugin, ModelError, ModelErrorKind,
    ModelProvider, ModelRequest, Observation, Observer, Plugin, Result, Role, Runner, ToolCall,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Loads [`ScriptedModel`]. Holds nothing itself.
pub struct ScriptedModelRunner;

/// The scripted model's code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptCode {
    /// The replies, in the order the agent receives them over its whole life. Required; an
    /// empty list is allowed and makes every request fail.
    replies: Vec<ScriptedReply>,
    /// How long each reply takes, in milliseconds, to give drains and cancels something to
    /// interrupt. Defaults to 0.
    #[serde(default)]
    delay_ms: u64,
}

/// One scripted assistant reply.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptedReply {
    /// The reply's text. Defaults to empty, which suits a reply that only calls tools.
    #[serde(default)]
    text: String,
    /// Tool calls the reply makes. Defaults to none; a reply without calls ends the step
    /// with an answer.
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
}

/// One scripted tool call.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptedCall {
    /// The tool's name. Not checked at load: a name no plugin provides reaches the driver,
    /// which answers it with an "unknown tool" result.
    name: String,
    /// The arguments, as a YAML value that's sent to the tool as JSON text. Defaults to
    /// null, which becomes `{}`.
    #[serde(default)]
    arguments: Value,
}

#[async_trait]
impl Runner for ScriptedModelRunner {
    fn name(&self) -> &str {
        "builtin.model.scripted"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: ScriptCode = env.parse_code(code)?;
        Ok(LoadedPlugin::stateless(Arc::new(ScriptedModel { code })))
    }
}

/// One agent's scripted model. Immutable; the reply index is derived from each request.
pub struct ScriptedModel {
    /// The parsed script.
    code: ScriptCode,
}

impl Plugin for ScriptedModel {
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        Some(self)
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    /// Picks the reply by counting assistant messages in the request, waits `delay_ms`,
    /// then returns it whole after emitting its text as one delta.
    ///
    /// The finish reason is `tool_calls` when the reply has calls and `stop` otherwise; no
    /// usage is reported. Returns a `Cancelled` error as soon as `cancel` fires during the
    /// delay, and a `Fatal` one when the script has no reply at this index.
    async fn complete(
        &self,
        request: ModelRequest,
        observer: Observer,
        cancel: CancellationToken,
    ) -> std::result::Result<AssistantMessage, ModelError> {
        let index = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let Some(scripted) = self.code.replies.get(index) else {
            return Err(ModelError::new(
                ModelErrorKind::Fatal,
                format!("the script has no reply {}", index + 1),
            ));
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(self.code.delay_ms)) => {}
            _ = cancel.cancelled() => return Err(ModelError::new(ModelErrorKind::Cancelled, "cancelled")),
        }
        if !scripted.text.is_empty() {
            observer.emit(Observation::TextDelta(scripted.text.clone()));
        }
        let tool_calls: Vec<ToolCall> = scripted
            .tool_calls
            .iter()
            .enumerate()
            .map(|(j, c)| ToolCall {
                id: CallId::new(format!("call-{index}-{j}")),
                name: c.name.clone(),
                arguments: if c.arguments.is_null() {
                    "{}".into()
                } else {
                    c.arguments.to_string()
                },
            })
            .collect();
        let finish = if tool_calls.is_empty() {
            Finish::Stop
        } else {
            Finish::ToolCalls
        };
        Ok(AssistantMessage {
            content: scripted.text.clone(),
            reasoning: None,
            tool_calls,
            finish,
            usage: None,
        })
    }
}
