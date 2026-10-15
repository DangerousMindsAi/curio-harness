//! `builtin.model.openai-compat`: a model provider for any OpenAI-compatible chat
//! completions API, such as DeepInfra's. Streams the reply and assembles tool calls.

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
/// credentials travel with each request.
pub struct OpenAiCompatRunner {
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
    /// API root, such as `https://api.deepinfra.com/v1/openai`. `/chat/completions` is appended.
    pub base_url: String,
    /// The provider's model id.
    pub model: String,
    /// Name of the secret holding the API key. It must also be in `grants.secrets`.
    #[serde(default)]
    pub api_key_secret: Option<String>,
    /// Seconds to wait for the response to start.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Seconds to wait between streamed chunks.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Extra fields merged into every request body, such as `top_p`.
    #[serde(default)]
    pub extra_body: Map<String, Value>,
}

fn default_connect_timeout() -> u64 {
    60
}
fn default_idle_timeout() -> u64 {
    120
}

#[async_trait]
impl Runner for OpenAiCompatRunner {
    fn name(&self) -> &str {
        "builtin.model.openai-compat"
    }

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

/// One agent's model provider.
pub struct OpenAiCompatModel {
    code: ModelCode,
    client: reqwest::Client,
    secrets: SecretScope,
}

impl Plugin for OpenAiCompatModel {
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        Some(self)
    }
}

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

/// The request body for one attempt.
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

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles streamed chunks into one reply.
#[derive(Default)]
pub struct ReplyAssembler {
    content: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish: Option<Finish>,
    usage: Option<Usage>,
    done: bool,
}

impl ReplyAssembler {
    /// Feeds one SSE `data:` payload. Emits text and reasoning deltas to `observer`.
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
