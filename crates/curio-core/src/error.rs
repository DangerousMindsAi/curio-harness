//! The error type shared by the runtime, plugins and runners.

use crate::ids::{AgentId, PluginId};
use std::fmt::Display;

/// Every failure the harness reports. Model failures inside a turn are not errors of this
/// type: they end the turn with [`crate::TurnEnd::Error`] and the agent keeps running.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("plugin `{plugin}`: {message}")]
    Plugin { plugin: PluginId, message: String },
    #[error("runner `{0}` is not registered or not enabled on this host")]
    UnknownRunner(String),
    #[error("log store: {0}")]
    Log(String),
    #[error(
        "the log of agent `{agent}` now belongs to a newer owner (epoch {current}, ours {ours})"
    )]
    Fenced {
        agent: AgentId,
        ours: u64,
        current: u64,
    },
    #[error(
        "the agent image is out of date: it was taken at log event {image}, but the log has {log} events"
    )]
    StaleImage { image: u64, log: u64 },
    #[error("agent `{0}` already exists")]
    AgentExists(AgentId),
    #[error("agent `{0}` has no log")]
    AgentNotFound(AgentId),
    #[error("message queue: {0}")]
    Queue(String),
    #[error("secret `{0}` is not granted to this plugin or not defined")]
    Secret(String),
    #[error("the agent task has stopped")]
    AgentGone,
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
