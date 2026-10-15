//! Core of curio-harness: the per-agent context, the plugin interfaces, the session log,
//! and the runtime that creates, thaws and recovers agents.
//!
//! The design this implements is `docs/agent-loop-proposal/README.md`. Every behavior,
//! including the loop itself, is provided by a plugin that a [`Runner`] loads from code.
//! All mutable state of one agent lives in its [`AgentCtx`], which is owned by one task.

pub mod agent;
pub mod config;
pub mod error;
pub mod event;
pub mod hook;
pub mod ids;
pub mod image;
pub mod log;
pub mod message;
pub mod plugin;
pub mod queue;
pub mod runtime;

pub use agent::{
    AgentCtx, AgentHandle, Answer, Command, DrainPoint, DrainResult, Exit, LoadKind, Observation,
    Observer, Phase, RunningAgent,
};
pub use config::{
    AgentConfig, PluginGrants, PluginSpec, ResumeConfig, RuntimeConfig, RuntimePluginSpec,
};
pub use error::{Error, Result};
pub use event::{
    Delivery, Event, History, InboxItem, QueuedMessage, Source, StepOutcome, TurnEnd, View,
};
pub use hook::{
    ErrorDecision, Flow, HookCx, HookEvent, HookName, ResponseVerdict, Subscription, ToolDecision,
    dispatch,
};
pub use ids::{AgentId, CallId, InboxItemId, PluginId, QueueMsgId, RequestId, TenantId, TurnId};
pub use image::AgentImage;
pub use log::{Journal, LogHeader, LogPosition, LogStore, OpenMode, OpenedLog, SessionLog};
pub use message::{
    AssistantMessage, ChatMessage, Concurrency, Finish, ModelError, ModelErrorKind, ModelRequest,
    Role, Route, ToolCall, ToolOutput, ToolResult, ToolSpec, Usage,
};
pub use plugin::{
    AgentSpec, CreateRequest, Driver, LoadEnv, LoadedPlugin, ModelProvider, Plugin, PluginEntry,
    PluginState, PluginTable, Provisioner, Runner, SecretScope, SecretStore, Tool, ToolCx,
    decode_state,
};
pub use queue::{MessageQueue, QueueConsumer};
pub use runtime::{RESTARTED_NOTICE, RunnerRegistry, Runtime, repair_after_crash};

/// Milliseconds since the Unix epoch, for timestamps recorded in logs and queues.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
