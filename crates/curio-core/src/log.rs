//! The session log interfaces and the [`Journal`] that keeps the log and the [`View`] in step.
//!
//! Every agent has one append-only session log: a [`LogHeader`] written at creation, then a
//! sequence of [`Event`]s. The log is the durable record the agent recovers from after a
//! crash, so anything the model sees must be in it first. Where logs live is a runtime-level
//! plugin, the [`LogStore`]; opening an agent's log gives its owner a [`SessionLog`] to
//! append to.
//!
//! Ownership is fenced by an epoch. Every open except [`OpenMode::Create`] raises the log's
//! epoch, and a [`SessionLog::sync`] from an older epoch fails with
//! [`crate::Error::Fenced`]. That's what makes moving an agent between hosts safe: once the
//! new host has opened the log, the old one can't write another event, even if it hasn't
//! noticed that it lost the agent.
//!
//! Appends are buffered and become durable only at a sync. The driver syncs at the save
//! points the design lists (before each model request, before each tool body, at the end of
//! a turn, after logging pulled messages and before acknowledging them, and at quiesce), so
//! a crash between two save points loses at most the events since the last one, and
//! recovery can still tell what may have happened.
//!
//! Inside an agent, the log is wrapped in a [`Journal`], which pairs it with the [`View`]
//! folded from it. [`Journal::append`] is the only way the live view changes, which keeps
//! the view and the log in agreement by construction.

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
/// Recorded in every image, and compared with the log's end on thaw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPosition {
    /// Number of events after the header.
    pub events: u64,
    /// The epoch of the owner that holds the log at this point.
    pub epoch: u64,
}

/// Written once when an agent is created, before any event. Recovery rebuilds the agent
/// from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogHeader {
    /// [`LOG_FORMAT`] at creation.
    pub format: u32,
    /// The agent's configuration, pinned for its whole life.
    pub config: AgentConfig,
    /// Each plugin's runner state version at creation.
    pub state_versions: BTreeMap<PluginId, u32>,
    /// When the agent was created, from [`crate::now_ms`].
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
    /// The log, ready for appending under the new epoch.
    pub log: Box<dyn SessionLog>,
    /// The header written at creation.
    pub header: LogHeader,
    /// Every event, oldest first, when recovering; empty otherwise.
    pub events: Vec<Event>,
}

/// Runtime-level service: where every agent's session log lives.
///
/// The store is shared by every agent and tenant in the process and isn't necessarily
/// local, so any host using the same store can open any agent's log by its id.
#[async_trait]
pub trait LogStore: Send + Sync {
    /// Opens an agent's log as its only writer. Fails with [`crate::Error::AgentNotFound`]
    /// when a mode other than `Create` finds no log.
    async fn open(&self, agent: &AgentId, mode: OpenMode) -> Result<OpenedLog>;

    /// Whether the agent has a log.
    async fn exists(&self, agent: &AgentId) -> Result<bool>;
}

/// One agent's log, held by its current owner.
///
/// Owned by the agent's [`Journal`], so only the agent's task appends to it.
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
///
/// The journal lives in [`crate::AgentCtx::journal`]. Hooks reach it through
/// [`crate::HookCx`], which can read the view and append but not sync.
pub struct Journal {
    /// The open log this journal appends to.
    log: Box<dyn SessionLog>,
    /// Everything folded from the log so far, including events not yet synced.
    view: View,
}

impl Journal {
    /// Pairs an opened log with the view it is consistent with: an empty view for a new or
    /// recovering agent, or the image's view on thaw.
    pub fn new(log: Box<dyn SessionLog>, view: View) -> Self {
        Self { log, view }
    }

    /// Logs an event and folds it into the view. If the log refuses the event, the view is
    /// left unchanged. The event isn't durable until the next [`Journal::sync`].
    pub fn append(&mut self, event: Event) -> Result<()> {
        self.log.append(&event)?;
        self.view.fold(&event);
        Ok(())
    }

    /// Folds an event that is already in the log, during crash recovery. Writes nothing.
    pub fn replay(&mut self, event: &Event) {
        self.view.fold(event);
    }

    /// Forces appended events to durable storage. See [`SessionLog::sync`] for fencing.
    pub async fn sync(&mut self) -> Result<()> {
        self.log.sync().await
    }

    /// Releases the log after a final sync. The journal mustn't be appended to afterwards.
    pub async fn release(&mut self) -> Result<()> {
        self.log.release().await
    }

    /// Where the log ends, including unsynced events.
    pub fn position(&self) -> LogPosition {
        self.log.position()
    }

    /// Everything derived from the log.
    pub fn view(&self) -> &View {
        &self.view
    }
}
