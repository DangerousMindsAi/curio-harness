//! The session log interfaces and the [`Journal`] that keeps the log and the [`View`] in step.

use crate::config::AgentConfig;
use crate::error::Result;
use crate::event::{Event, View};
use crate::ids::{AgentId, PluginId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Version of the log header and event encoding.
pub const LOG_FORMAT: u32 = 1;

/// A point in an agent's log: how many events it holds, and the owner epoch that wrote them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPosition {
    pub events: u64,
    pub epoch: u64,
}

/// Written once when an agent is created.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogHeader {
    pub format: u32,
    pub config: AgentConfig,
    /// Each plugin's runner state version at creation.
    pub state_versions: BTreeMap<PluginId, u32>,
    pub created_at_ms: u64,
}

/// How to open a log. Every mode except `Create` raises the epoch, which fences off any
/// earlier owner.
#[derive(Clone, Debug)]
pub enum OpenMode {
    /// Create a new log. Fails if one exists.
    Create(LogHeader),
    /// Continue an existing log from a quiesced image. Fails with
    /// [`crate::Error::StaleImage`] unless the log ends exactly at `position`.
    ContinueFrom(LogPosition),
    /// Take over an existing log after a crash and read it in full.
    Recover,
}

/// An opened log. `events` is filled only for [`OpenMode::Recover`].
pub struct OpenedLog {
    pub log: Box<dyn SessionLog>,
    pub header: LogHeader,
    pub events: Vec<Event>,
}

/// Runtime-level service: where every agent's session log lives.
#[async_trait]
pub trait LogStore: Send + Sync {
    /// Opens an agent's log as its only writer.
    async fn open(&self, agent: &AgentId, mode: OpenMode) -> Result<OpenedLog>;

    /// Whether the agent has a log.
    async fn exists(&self, agent: &AgentId) -> Result<bool>;
}

/// One agent's log, held by its current owner.
#[async_trait]
pub trait SessionLog: Send + Sync {
    /// Where the log ends, counting events appended but not yet synced.
    fn position(&self) -> LogPosition;

    /// Buffers one event. It becomes durable at the next [`SessionLog::sync`].
    fn append(&mut self, event: &Event) -> Result<()>;

    /// Makes every appended event durable. Fails with [`crate::Error::Fenced`] when a newer
    /// owner has opened the log; the events buffered since the last sync are then lost.
    async fn sync(&mut self) -> Result<()>;

    /// Gives up ownership after a final sync, so the next owner need not wait for a lease.
    async fn release(&mut self) -> Result<()>;
}

/// An agent's log together with the view folded from it. Appending is the only way the
/// view changes while the agent runs.
pub struct Journal {
    log: Box<dyn SessionLog>,
    view: View,
}

impl Journal {
    /// Pairs an opened log with the view it is consistent with.
    pub fn new(log: Box<dyn SessionLog>, view: View) -> Self {
        Self { log, view }
    }

    /// Logs an event and folds it into the view.
    pub fn append(&mut self, event: Event) -> Result<()> {
        self.log.append(&event)?;
        self.view.fold(&event);
        Ok(())
    }

    /// Folds an event that is already in the log, during crash recovery.
    pub fn replay(&mut self, event: &Event) {
        self.view.fold(event);
    }

    /// Forces appended events to durable storage.
    pub async fn sync(&mut self) -> Result<()> {
        self.log.sync().await
    }

    /// Releases the log after a final sync.
    pub async fn release(&mut self) -> Result<()> {
        self.log.release().await
    }

    /// Where the log ends.
    pub fn position(&self) -> LogPosition {
        self.log.position()
    }

    /// Everything derived from the log.
    pub fn view(&self) -> &View {
        &self.view
    }
}
