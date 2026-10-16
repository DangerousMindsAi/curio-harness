//! Conversation, model and tool data types.
//!
//! These are the values that pass between the driver, model providers and tools, and most
//! of them are also logged. The model-visible conversation is a list of [`ChatMessage`]s,
//! held in [`crate::History`] and built only by folding events. Each step, the driver turns
//! that history into a [`ModelRequest`] (with a [`Route`] that `request` listeners may
//! adjust and the [`ToolSpec`]s of every tool plugin) and hands it to a
//! [`crate::ModelProvider`], which returns an [`AssistantMessage`] or a [`ModelError`].
//!
//! Each [`ToolCall`] in a reply goes through the tool pipeline: a [`crate::Tool`] body
//! returns a [`ToolOutput`], and the driver turns it into a [`ToolResult`], which
//! `tool_post` listeners may rewrite before it's logged and becomes a tool message in the
//! history. Calls that never run (denied, cancelled, or cut off by a crash) get a
//! [`ToolResult::synthetic`] result instead, so every call the model made gets an answer.

use crate::ids::{CallId, PluginId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who a [`ChatMessage`] is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The single system message built from every plugin's system prompt.
    System,
    /// Input admitted from the inbox: queued messages, plugin context and system notices.
    User,
    /// A model reply.
    Assistant,
    /// The result of one tool call.
    Tool,
}

/// One message of the model-visible conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who the message is from.
    pub role: Role,
    /// The message text; empty for an assistant message that only calls tools.
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
///
/// The arguments stay unparsed here because the model may produce invalid JSON; the driver
/// parses them when it prepares the call, and a `tool_pre` listener may rewrite them first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The model's id for the call.
    pub id: CallId,
    /// The tool's name, as in its [`ToolSpec`].
    pub name: String,
    /// The arguments as JSON text.
    pub arguments: String,
}

/// Why the model stopped producing a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    /// The model finished its answer.
    Stop,
    /// The model stopped to have tools run.
    ToolCalls,
    /// The reply hit the output token limit. The driver drops any tool calls in such a
    /// reply, since they may be cut off, and ends the turn with `MaxTokens`.
    Length,
    /// The provider filtered the reply.
    ContentFilter,
    /// Any other reason the provider reported.
    Other,
}

/// Token counts reported by the provider. Summed into [`crate::event::Counters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the request.
    pub prompt_tokens: u64,
    /// Tokens in the reply.
    pub completion_tokens: u64,
}

/// One complete model reply. Logged as [`crate::Event::AssistantReply`] before any of its
/// tool calls run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// The reply text.
    pub content: String,
    /// Reasoning text from models that expose it. Logged, but not sent back to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Tools the model asked to run, in the model's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Why the reply ended.
    pub finish: Finish,
    /// Token counts, when the provider reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Whether a tool may run alongside other calls from the same reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// May overlap with neighbouring parallel calls from the same reply.
    Parallel,
    /// Runs alone, after the calls before it and before the calls after it.
    Exclusive,
}

/// What the model is told about a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model calls the tool by. Unique within an agent.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
    /// How the driver may schedule calls to this tool. Not sent to the model.
    pub concurrency: Concurrency,
}

/// What a tool body returns. The driver adds the call's id and name to make a
/// [`ToolResult`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The text the model sees.
    pub content: String,
    /// Whether the tool failed. A failure is reported to the model; it doesn't end the turn.
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
    /// The call this answers.
    pub call_id: CallId,
    /// The tool's name, kept for display and diagnosis.
    pub name: String,
    /// The text the model sees.
    pub content: String,
    /// Whether the call failed or never ran.
    pub is_error: bool,
    /// Set by a `tool_post` listener to end the turn after this step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub concludes_turn: bool,
}

impl ToolResult {
    /// A result that did not come from running the tool, such as a denial or crash repair.
    /// Always marked as an error, with `content` as the model-visible explanation.
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
///
/// The driver builds a fresh route from its own config for every attempt, so a listener's
/// change applies to that attempt only. The route is not logged; the request it shapes is
/// rebuilt from the history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// The plugin that provides the model.
    pub provider: PluginId,
    /// Output token limit; the provider's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature; the provider's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// One model request, built only from the agent's history.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// The system message followed by the conversation, from
    /// [`crate::History::to_request_messages`].
    pub messages: Vec<ChatMessage>,
    /// Every tool the agent has, in config order.
    pub tools: Vec<ToolSpec>,
    /// Where and how to send the request.
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
    /// The turn was cancelled while the request was in flight. The driver ends the step as
    /// cancelled without logging a failed attempt.
    Cancelled,
}

/// A failed model attempt. The driver logs it as [`crate::Event::AttemptFailed`] (unless
/// it's a cancellation) and passes it to `request_error` listeners.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?} model error{}: {message}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct ModelError {
    /// What kind of failure, for retry decisions.
    pub kind: ModelErrorKind,
    /// A description for logs and the user.
    pub message: String,
    /// The HTTP status, when the failure came with one.
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
