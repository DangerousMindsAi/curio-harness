//! Conversation, model and tool data types.

use crate::ids::{CallId, PluginId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who a [`ChatMessage`] is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message of the model-visible conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Tool calls an assistant message made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a tool message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<CallId>,
}

impl ChatMessage {
    /// A message with only a role and text.
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// A tool call as the model requested it. `arguments` is the model's raw JSON text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: String,
}

/// Why the model stopped producing a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

/// Token counts reported by the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// One complete model reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    /// Reasoning text from models that expose it. Logged, but not sent back to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    pub finish: Finish,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Whether a tool may run alongside other calls from the same reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    Parallel,
    Exclusive,
}

/// What the model is told about a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
    pub concurrency: Concurrency,
}

/// What a tool body returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result; the text is shown to the model.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// The committed result of one tool call, as logged and shown to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: CallId,
    pub name: String,
    pub content: String,
    pub is_error: bool,
    /// Set by a `tool_post` listener to end the turn after this step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub concludes_turn: bool,
}

impl ToolResult {
    /// A result that did not come from running the tool, such as a denial or crash repair.
    pub fn synthetic(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error: true,
            concludes_turn: false,
        }
    }
}

/// Request options that `request` hook listeners may change before each attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// The plugin that provides the model.
    pub provider: PluginId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// One model request, built only from the agent's history.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub route: Route,
}

/// How a failed model attempt should be treated by retry policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    /// Rate limits, server errors and network failures: worth retrying.
    Transient,
    /// The request was too large for the model.
    ContextOverflow,
    /// Not worth retrying, such as a bad API key or an invalid request.
    Fatal,
    /// The turn was cancelled while the request was in flight.
    Cancelled,
}

/// A failed model attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?} model error{}: {message}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl ModelError {
    /// An error of the given kind without an HTTP status.
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
        }
    }
}
