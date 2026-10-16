//! Core of curio-harness: the per-agent context, the plugin interfaces, the session log,
//! and the runtime that creates, thaws and recovers agents.
//!
//! The design this implements is `docs/agent-loop-proposal/README.md`. Every behavior,
//! including the loop itself, is provided by a plugin that a [`Runner`] loads from code.
//! This crate holds no behavior of its own beyond bookkeeping: the default loop, the model
//! clients, the queue and log store backends and every tool live in `curio-builtins`, and
//! reach the core only through the traits defined here.
//!
//! The modules fit together like this. [`runtime`] is the per-process entry point: a
//! [`Runtime`] owns the [`RunnerRegistry`] and the three runtime-level plugins (log store,
//! provisioner, secret store), and turns a create request, an [`AgentImage`] or a crashed
//! agent's log into a [`RunningAgent`]. It does that by loading every plugin named in the
//! agent's [`AgentConfig`] through its runner ([`plugin`]), opening the agent's session log
//! ([`log`]), building an [`AgentCtx`] ([`agent`]) and spawning a task that hands the
//! context to the agent's [`Driver`] plugin. From then on the driver runs the loop: it pulls
//! messages from the agent's queue ([`queue`]), appends [`Event`]s to the log ([`event`]),
//! calls the model and tools ([`message`] holds the data they exchange), and dispatches
//! hooks to the other plugins ([`hook`]). When a drain reaches a safe point, the context is
//! serialized into an [`AgentImage`] ([`image`]) that a runtime on any host can thaw.
//!
//! Two invariants hold across the crate. All mutable state of one agent lives in its
//! [`AgentCtx`], which is owned by one task; outside callers reach it only through an
//! [`AgentHandle`]. And everything the model sees is derived from the session log: the
//! [`View`] changes only by folding logged events, so replaying the log after a crash gives
//! back the same conversation the live agent had.

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
///
/// Timestamps are for diagnosis only; nothing orders events by them. If the system clock
/// reads earlier than the epoch, this returns 0 instead of failing.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
