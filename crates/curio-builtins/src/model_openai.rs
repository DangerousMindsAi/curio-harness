//! `builtin.model.openai-compat`: a model provider for any OpenAI-compatible chat
//! completions API, such as DeepInfra's.
//!
//! It's a per-agent plugin that implements [`ModelProvider`] and subscribes to no hooks.
//! The driver calls [`ModelProvider::complete`] once per attempt with a request built from
//! the agent's history; this plugin turns it into one streaming `POST <base_url>/chat/completions`,
//! forwards text and reasoning deltas to the agent's observer as they arrive, and returns
//! the assembled [`AssistantMessage`]. Nothing it streams is logged: the driver logs the
//! complete reply, and a reply cut off mid-stream is simply a failed attempt.
//!
//! ```yaml
//! - id: model
//!   runner: builtin.model.openai-compat
//!   code:
//!     base_url: https://api.deepinfra.com/v1/openai
//!     model: deepseek-ai/DeepSeek-V3.2
//!     api_key_secret: deepinfra         # optional; a name in the host's secret store
//!     connect_timeout_secs: 60          # optional
//!     idle_timeout_secs: 120            # optional
//!     extra_body: { top_p: 0.9 }        # optional
//!   grants:
//!     hosts: [api.deepinfra.com]        # must include base_url's host
//!     secrets: [deepinfra]              # must include api_key_secret
//! ```
//!
//! Grants are checked at load: the base URL's host must be in `grants.hosts`, and the API
//! key must resolve, so a missing key fails agent creation instead of the first turn. The
//! key itself is never stored in the plugin. It's resolved again through the plugin's
//! [`SecretScope`] for every request and sent as a bearer token, which keeps it out of
//! logs, images and memory between requests.
//!
//! Failures come back as [`ModelError`]s with a kind that retry policies read. Rate limits,
//! timeouts, server errors, network errors and malformed or truncated streams are
//! `Transient`; a 4xx whose body mentions the context length is `ContextOverflow`; any
//! other 4xx is `Fatal`; a cancel is `Cancelled`. This plugin never retries by itself;
//! `builtin.retry` does that through the `request_error` hook.

use async_trait::async_trait;
use curio_core::{
    AssistantMessage, CallId, ChatMessage, Error, Finish, LoadEnv, LoadedPlugin, ModelError,
    ModelErrorKind, ModelProvider, ModelRequest, Observation, Observer, Plugin, Result, Role,
    Runner, SecretScope, ToolCall, Usage,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Loads [`OpenAiCompatModel`]. Holds one HTTP connection pool shared by every agent;
/// credentials travel with each request, so sharing the pool leaks nothing between tenants.
pub struct OpenAiCompatRunner {
    /// The shared pool. Cloning a `reqwest::Client` shares it.
    client: reqwest::Client,
}

impl OpenAiCompatRunner {
    /// A runner with its own connection pool.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiCompatRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// The model plugin's code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCode {
    /// API root, such as `https://api.deepinfra.com/v1/openai`. `/chat/completions` is
    /// appended, after trimming any trailing slash. Required; must parse as a URL with a
    /// host, and that host must be in `grants.hosts`.
    pub base_url: String,
    /// The provider's model id, sent as the body's `model` field. Required.
    pub model: String,
    /// Name of the secret holding the API key, looked up in the agent's tenant. It must
    /// also be in `grants.secrets`. Defaults to none, which sends no `Authorization`
    /// header (for local servers that don't need one).
    #[serde(default)]
    pub api_key_secret: Option<String>,
    /// Seconds to wait from sending the request until the response headers arrive.
    /// Defaults to 60. Expiry is a transient error.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Seconds to wait for each streamed chunk once the response has started. Defaults to
    /// 120. Expiry is a transient "the stream stalled" error.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Extra fields merged into every request body, such as `top_p`. Defaults to empty.
    /// They're applied last, so they can also override the fields this plugin sets,
    /// including `model` and `stream`; overriding `stream` breaks the reply parsing.
    #[serde(default)]
    pub extra_body: Map<String, Value>,
}

/// Default for [`ModelCode::connect_timeout_secs`].
fn default_connect_timeout() -> u64 {
    60
}
/// Default for [`ModelCode::idle_timeout_secs`].
fn default_idle_timeout() -> u64 {
    120
}

#[async_trait]
impl Runner for OpenAiCompatRunner {
    fn name(&self) -> &str {
        "builtin.model.openai-compat"
    }

    /// Parses the code, checks the base URL's host against `grants.hosts`, and resolves
    /// the API key once to prove it exists and is granted. Each check fails with an error
    /// attributed to this plugin, so agent creation stops there.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: ModelCode = env.parse_code(code)?;
        let url = reqwest::Url::parse(&code.base_url)
            .map_err(|e| Error::plugin(&env.plugin, format!("invalid base_url: {e}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| Error::plugin(&env.plugin, "base_url has no host"))?;
        env.require_host(host)?;
        if let Some(name) = &code.api_key_secret {
            // Fail at load if the key is missing; the value is resolved again per request.
            env.secrets
                .resolve(name)
                .await
                .map_err(|e| Error::plugin(&env.plugin, e))?;
        }
        Ok(LoadedPlugin::stateless(Arc::new(OpenAiCompatModel {
            code,
            client: self.client.clone(),
            secrets: env.secrets.clone(),
        })))
    }
}

/// One agent's model provider. Immutable; holds no conversation state, since every
/// request carries the whole history.
pub struct OpenAiCompatModel {
    /// The validated code.
    code: ModelCode,
    /// A handle on the runner's shared connection pool.
    client: reqwest::Client,
    /// This plugin's view of the secret store: its tenant, its granted names.
    secrets: SecretScope,
}

impl Plugin for OpenAiCompatModel {
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        Some(self)
    }
}

/// Converts one history message to the chat completions wire format.
///
/// An assistant message with tool calls carries them as `function` calls, with `content`
/// set to null when it has no text (some providers reject an empty string there). A tool
/// message carries the `tool_call_id` it answers. Reasoning text isn't part of
/// [`ChatMessage`], so it's never sent back to the model.
fn wire_message(m: &ChatMessage) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut out = json!({ "role": role, "content": m.content });
    if !m.tool_calls.is_empty() {
        if m.content.is_empty() {
            out["content"] = Value::Null;
        }
        out["tool_calls"] = m
            .tool_calls
            .iter()
            .map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } }))
            .collect();
    }
    if let Some(id) = &m.tool_call_id {
        out["tool_call_id"] = json!(id);
    }
    out
}

/// Builds the JSON body for one attempt.
///
/// Always asks for a stream with usage in the final chunk. `tools` is present only when
/// the agent has tools, and `max_tokens` and `temperature` only when the route sets them.
/// The entries of `extra` are copied in last and win over everything else.
pub fn request_body(model: &str, request: &ModelRequest, extra: &Map<String, Value>) -> Value {
    let mut body = json!({
        "model": model,
        "messages": request.messages.iter().map(wire_message).collect::<Vec<_>>(),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } }))
            .collect();
    }
    if let Some(n) = request.route.max_tokens {
        body["max_tokens"] = json!(n);
    }
    if let Some(t) = request.route.temperature {
        body["temperature"] = json!(t);
    }
    for (k, v) in extra {
        body[k] = v.clone();
    }
    body
}

/// Sorts a failed HTTP response into a retry class.
///
/// 408, 429 and every 5xx are transient. Otherwise the body is searched, case-insensitively,
/// for the phrases providers use for an oversized prompt ("context length", "context
/// window", "too many tokens"), which make it a context overflow. Anything else is fatal.
fn classify_status(status: u16, body: &str) -> ModelErrorKind {
    let lower = body.to_lowercase();
    if status == 429 || status == 408 || status >= 500 {
        ModelErrorKind::Transient
    } else if lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("too many tokens")
    {
        ModelErrorKind::ContextOverflow
    } else {
        ModelErrorKind::Fatal
    }
}

/// A tool call while its pieces are still streaming in.
#[derive(Default)]
struct PartialCall {
    /// The provider's call id. Usually sent once, in the call's first chunk; empty until
    /// then.
    id: String,
    /// The function name, concatenated from every chunk that carries part of it.
    name: String,
    /// The JSON argument text, concatenated from every chunk; not parsed here.
    arguments: String,
}

/// Assembles the `data:` payloads of one streamed response into an [`AssistantMessage`].
///
/// Feed it each payload in order with [`ReplyAssembler::feed`], then call
/// [`ReplyAssembler::finish`]. It's public so the parsing can be tested without a server.
#[derive(Default)]
pub struct ReplyAssembler {
    /// Answer text so far.
    content: String,
    /// Reasoning text so far, from `reasoning_content` or `reasoning` deltas.
    reasoning: String,
    /// Tool calls by their stream `index`.
    calls: Vec<PartialCall>,
    /// The last finish reason seen; `None` until the provider sends one.
    finish: Option<Finish>,
    /// Token usage, usually from a final chunk with no choices.
    usage: Option<Usage>,
    /// Set by the `[DONE]` sentinel; the HTTP loop stops reading once it's set.
    done: bool,
}

impl ReplyAssembler {
    /// Feeds one SSE `data:` payload. Emits text and reasoning deltas to `observer`.
    ///
    /// Only the first choice is read. Tool call fragments are merged by their `index`
    /// (falling back to "a new call" when a provider leaves it out). Fails with a transient
    /// error when the payload isn't JSON or is an in-stream `error` object, since both
    /// usually mean the provider broke off mid-reply.
    pub fn feed(&mut self, data: &str, observer: &Observer) -> std::result::Result<(), ModelError> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            ModelError::new(
                ModelErrorKind::Transient,
                format!("unreadable stream chunk: {e}"),
            )
        })?;
        if let Some(err) = chunk.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_string();
            return Err(ModelError::new(ModelErrorKind::Transient, message));
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(Usage {
                prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                completion_tokens: u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.content.push_str(text);
            observer.emit(Observation::TextDelta(text.to_string()));
        }
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str);
        if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
            self.reasoning.push_str(text);
            observer.emit(Observation::ReasoningDelta(text.to_string()));
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(self.calls.len() as u64) as usize;
            while self.calls.len() <= index {
                self.calls.push(PartialCall::default());
            }
            let slot = &mut self.calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                slot.id = id.to_string();
            }
            let function = call.get("function").unwrap_or(&Value::Null);
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                slot.name.push_str(name);
            }
            if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                slot.arguments.push_str(args);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(match reason {
                "stop" => Finish::Stop,
                "tool_calls" | "function_call" => Finish::ToolCalls,
                "length" => Finish::Length,
                "content_filter" => Finish::ContentFilter,
                _ => Finish::Other,
            });
        }
        Ok(())
    }

    /// The complete reply. Fails if the stream ended before the provider said it finished.
    ///
    /// Calls that never received a name are dropped. A call the provider sent without an
    /// id gets `call_<index>`; these fallback ids repeat from one reply to the next, which
    /// is harmless because a step's calls are all resolved before the next request.
    /// Reasoning is `None` when the stream had none. The missing-finish error is transient,
    /// worded to say whether `[DONE]` arrived.
    pub fn finish(self) -> std::result::Result<AssistantMessage, ModelError> {
        let Some(finish) = self.finish else {
            let message = if self.done {
                "the stream ended without a finish reason"
            } else {
                "the stream ended early"
            };
            return Err(ModelError::new(ModelErrorKind::Transient, message));
        };
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| ToolCall {
                id: CallId::new(if c.id.is_empty() {
                    format!("call_{i}")
                } else {
                    c.id
                }),
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        Ok(AssistantMessage {
            content: self.content,
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls,
            finish,
            usage: self.usage,
        })
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatModel {
    /// Runs one attempt: resolve the key, send, check the status, then read the event
    /// stream line by line until `[DONE]` or the connection closes.
    ///
    /// Cancellation is checked while waiting for the response headers and for each chunk,
    /// and returns a `Cancelled` error at once (dropping the connection). Reading the body
    /// of an error response isn't covered by the cancel token or either timeout.
    async fn complete(
        &self,
        request: ModelRequest,
        observer: Observer,
        cancel: CancellationToken,
    ) -> std::result::Result<AssistantMessage, ModelError> {
        let cancelled = || ModelError::new(ModelErrorKind::Cancelled, "cancelled");
        let mut http = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.code.base_url.trim_end_matches('/')
            ))
            .json(&request_body(
                &self.code.model,
                &request,
                &self.code.extra_body,
            ));
        if let Some(name) = &self.code.api_key_secret {
            let key = self
                .secrets
                .resolve(name)
                .await
                .map_err(|e| ModelError::new(ModelErrorKind::Fatal, e.to_string()))?;
            http = http.bearer_auth(key);
        }

        let send = tokio::time::timeout(
            Duration::from_secs(self.code.connect_timeout_secs),
            http.send(),
        );
        let response = tokio::select! {
            r = send => r,
            _ = cancel.cancelled() => return Err(cancelled()),
        };
        let response = match response {
            Err(_) => {
                return Err(ModelError::new(
                    ModelErrorKind::Transient,
                    "timed out waiting for the response",
                ));
            }
            Ok(Err(e)) => return Err(ModelError::new(ModelErrorKind::Transient, e.to_string())),
            Ok(Ok(r)) => r,
        };
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            let mut text: String = body.chars().take(2000).collect();
            if text.is_empty() {
                text = format!("HTTP {status}");
            }
            return Err(ModelError {
                kind: classify_status(status, &body),
                message: text,
                status: Some(status),
            });
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut reply = ReplyAssembler::default();
        let idle = Duration::from_secs(self.code.idle_timeout_secs);
        loop {
            let next = tokio::select! {
                n = tokio::time::timeout(idle, stream.next()) => n,
                _ = cancel.cancelled() => return Err(cancelled()),
            };
            let chunk = match next {
                Err(_) => {
                    return Err(ModelError::new(
                        ModelErrorKind::Transient,
                        "the stream stalled",
                    ));
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    return Err(ModelError::new(ModelErrorKind::Transient, e.to_string()));
                }
                Ok(Some(Ok(bytes))) => bytes,
            };
            // A network chunk can end mid-line, so only complete lines are parsed and the
            // rest waits in the buffer for the next chunk.
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(data) = line.strip_prefix("data:") {
                    reply.feed(data.trim_start(), &observer)?;
                }
            }
            if reply.done {
                break;
            }
        }
        reply.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text deltas concatenate, tool call fragments with the same index merge into one call
    /// (id and name from the first fragment, arguments from all of them), the finish reason
    /// and the trailing usage chunk are kept, and the first text delta reaches the observer.
    #[test]
    fn assembles_text_and_streamed_tool_calls() {
        let observer = Observer::new();
        let mut rx = observer.subscribe();
        let mut a = ReplyAssembler::default();
        for data in [
            r#"{"choices":[{"delta":{"role":"assistant","content":"Let me "}}]}"#,
            r#"{"choices":[{"delta":{"content":"check."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"clock","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"shell","arguments":"{\"command\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7}}"#,
            "[DONE]",
        ] {
            a.feed(data, &observer).unwrap();
        }
        let reply = a.finish().unwrap();
        assert_eq!(reply.content, "Let me check.");
        assert_eq!(reply.finish, Finish::ToolCalls);
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[1].arguments, r#"{"command":"ls"}"#);
        assert_eq!(
            reply.usage,
            Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 7
            })
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Observation::TextDelta("Let me ".into())
        );
    }

    /// A stream that stops before any finish reason fails as a transient error, so a retry
    /// policy can try again instead of treating the partial text as a reply.
    #[test]
    fn a_stream_without_finish_reason_is_transient() {
        let mut a = ReplyAssembler::default();
        a.feed(
            r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            &Observer::new(),
        )
        .unwrap();
        assert_eq!(a.finish().unwrap_err().kind, ModelErrorKind::Transient);
    }

    /// HTTP failures map to the retry classes: 429 and 5xx are transient, an auth failure is
    /// fatal, and a 400 that mentions the context length is a context overflow.
    #[test]
    fn status_codes_map_to_retry_classes() {
        assert_eq!(classify_status(429, ""), ModelErrorKind::Transient);
        assert_eq!(classify_status(503, ""), ModelErrorKind::Transient);
        assert_eq!(classify_status(401, "bad key"), ModelErrorKind::Fatal);
        assert_eq!(
            classify_status(400, "maximum context length is 8192"),
            ModelErrorKind::ContextOverflow
        );
    }
}
