//! `builtin.model.scripted`: a model provider that plays back a fixed list of replies.
//! For tests and offline demos. The reply it gives is chosen by how many assistant
//! messages the request already holds, so it needs no state and behaves the same after
//! thaw or recovery.

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

/// Loads [`ScriptedModel`].
pub struct ScriptedModelRunner;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptCode {
    replies: Vec<ScriptedReply>,
    /// How long each reply takes, to give drains and cancels something to interrupt.
    #[serde(default)]
    delay_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptedReply {
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptedCall {
    name: String,
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

/// Plays back scripted replies.
pub struct ScriptedModel {
    code: ScriptCode,
}

impl Plugin for ScriptedModel {
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        Some(self)
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
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
