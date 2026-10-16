//! The error type shared by the runtime, plugins and runners.
//!
//! [`Error`] covers everything that stops an operation of the harness itself: bad
//! configuration, a plugin that fails to load or misbehaves, a log store or queue failure,
//! losing ownership of a log, or an agent task that's gone. When the driver returns one of
//! these, the agent task ends and any drain waiters get [`crate::DrainResult::Failed`]; the
//! agent can then be recovered from its log.
//!
//! Failures the agent is expected to live with aren't [`Error`]s. A failed model attempt is
//! a [`crate::ModelError`], which the retry policy sees through the `request_error` hook and
//! which at worst ends the turn with [`crate::TurnEnd::Error`]. A failed tool body returns a
//! [`crate::ToolOutput`] with `is_error` set, which the model reads like any other result.

use crate::ids::{AgentId, PluginId};
use std::fmt::Display;

/// Every failure the harness reports. Model failures inside a turn are not errors of this
/// type: they end the turn with [`crate::TurnEnd::Error`] and the agent keeps running.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration is invalid: an unknown field, a missing service, a duplicate id,
    /// an image in the wrong format. Raised at load time, before an agent does any work.
    #[error("configuration error: {0}")]
    Config(String),
    /// A failure attributed to one plugin, such as invalid code, a hook it isn't granted,
    /// or an error from one of its hooks.
    #[error("plugin `{plugin}`: {message}")]
    Plugin { plugin: PluginId, message: String },
    /// A config names a runner that isn't registered in this process, or that the host's
    /// `runners` list doesn't enable.
    #[error("runner `{0}` is not registered or not enabled on this host")]
    UnknownRunner(String),
    /// The log store failed to read or write a log.
    #[error("log store: {0}")]
    Log(String),
    /// Another host opened this agent's log after we did and raised its epoch. Our pending
    /// appends weren't written, and this owner must stop: the agent now runs elsewhere.
    #[error(
        "the log of agent `{agent}` now belongs to a newer owner (epoch {current}, ours {ours})"
    )]
    Fenced {
        agent: AgentId,
        /// The epoch this owner opened the log with.
        ours: u64,
        /// The epoch the log store holds now.
        current: u64,
    },
    /// Thaw was given an image that doesn't match the end of the log: the agent kept
    /// running (or was recovered) after the image was taken, so the image is out of date.
    #[error(
        "the agent image is out of date: it was taken at log event {image}, but the log has {log} events"
    )]
    StaleImage {
        /// Event count recorded in the image.
        image: u64,
        /// Event count in the log.
        log: u64,
    },
    /// Create was asked for an agent id that already has a log.
    #[error("agent `{0}` already exists")]
    AgentExists(AgentId),
    /// Thaw or recover was asked for an agent id with no log.
    #[error("agent `{0}` has no log")]
    AgentNotFound(AgentId),
    /// The message queue plugin failed, or the agent isn't attached to its queue.
    #[error("message queue: {0}")]
    Queue(String),
    /// A plugin asked for a secret it isn't granted, or the store doesn't define it.
    #[error("secret `{0}` is not granted to this plugin or not defined")]
    Secret(String),
    /// A command was sent to an agent whose task has ended, or the task ended before it
    /// replied.
    #[error("the agent task has stopped")]
    AgentGone,
    /// An I/O error, typically from a file-backed plugin.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A JSON encoding or decoding error, typically for a state slot, image or log line.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Result with [`Error`] as the default error.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// A failure attributed to one plugin.
    pub fn plugin(plugin: &PluginId, message: impl Display) -> Self {
        Error::Plugin {
            plugin: plugin.clone(),
            message: message.to_string(),
        }
    }

    /// A configuration problem, reported at load time.
    pub fn config(message: impl Display) -> Self {
        Error::Config(message.to_string())
    }
}
