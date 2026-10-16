//! `builtin.logstore.file`: session logs as JSON Lines files on local disk, with an epoch
//! file per agent for fencing.
//!
//! This is a runtime-level plugin: the host's startup file names it as the `log_store`,
//! and it implements [`LogStore`] with no hooks and no state. The runtime opens a log in
//! one of three modes. [`OpenMode::Create`] writes a new log for
//! [`curio_core::Runtime::create`], [`OpenMode::ContinueFrom`] reopens it for
//! [`curio_core::Runtime::thaw`], and [`OpenMode::Recover`] reopens it and reads every
//! event back for [`curio_core::Runtime::recover`]. Each open returns a [`SessionLog`] that
//! the agent's journal appends to and syncs at the driver's save points.
//!
//! ```yaml
//! log_store:
//!   runner: builtin.logstore.file
//!   code: { dir: data/logs }   # relative to the startup file's directory
//! ```
//!
//! Layout under `dir`: `<agent>/log.jsonl` (the header on line 1, then one event per line,
//! each tagged with the epoch that wrote it) and `<agent>/epoch` (the current owner's
//! epoch). Appends are buffered in memory and reach the file only at a sync, as one write
//! followed by `fdatasync`. A crash in the middle of that write leaves a torn last line,
//! which the next open cuts off.
//!
//! Fencing works through the epoch file. Creating a log sets the epoch to 1, and every
//! later open raises it, which makes the new owner the only one whose syncs succeed: before
//! each write, a sync rereads the epoch file and fails with [`Error::Fenced`] if it no
//! longer holds its own epoch. The check and the write are two steps, which is enough for
//! one machine; a shared store would make them one conditional write. Two processes on one
//! machine can still interleave: an owner that passes the check just before a takeover
//! writes one more batch after it.
//!
//! There's no lease. [`SessionLog::release`] is only a final sync, and nothing stops
//! another host from opening a log that a live agent still holds; the epoch is what keeps
//! the old owner from writing afterwards.

use crate::fsutil::{path_component, write_atomic};
use async_trait::async_trait;
use curio_core::{
    AgentId, Error, Event, LoadEnv, LoadedPlugin, LogHeader, LogPosition, LogStore, OpenMode,
    OpenedLog, Plugin, Result, Runner, SessionLog,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Loads [`FileLogStore`]. Holds nothing itself.
pub struct FileLogStoreRunner;

/// The log store's code.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreCode {
    /// The directory every agent's log lives under. Required; created at load if missing.
    dir: PathBuf,
}

#[async_trait]
impl Runner for FileLogStoreRunner {
    fn name(&self) -> &str {
        "builtin.logstore.file"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: StoreCode = env.parse_code(code)?;
        let dir = env.resolve_path(&code.dir);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(LoadedPlugin::stateless(Arc::new(FileLogStore { dir })))
    }
}

/// Session logs in a directory, one subdirectory per agent. Immutable; every open works
/// from the files alone, so several runtimes in one process (or several processes) can
/// share a directory, fenced by the epoch file.
pub struct FileLogStore {
    /// The resolved store directory.
    dir: PathBuf,
}

impl Plugin for FileLogStore {
    fn as_log_store(&self) -> Option<&dyn LogStore> {
        Some(self)
    }
}

/// Line 1 of a log file: `{"header": ...}`.
#[derive(Serialize, Deserialize)]
struct HeaderLine {
    /// The header the runtime built at creation.
    header: LogHeader,
}

/// Every later line of a log file: `{"epoch": N, "event": ...}`.
#[derive(Serialize, Deserialize)]
struct EventLine {
    /// The epoch of the owner that wrote the line. Recorded for diagnosis; reading ignores
    /// it.
    epoch: u64,
    /// The event itself.
    event: Event,
}

/// The files that make up one agent's log.
struct Paths {
    /// `<dir>/<agent>`.
    dir: PathBuf,
    /// `<dir>/<agent>/log.jsonl`.
    log: PathBuf,
    /// `<dir>/<agent>/epoch`.
    epoch: PathBuf,
}

impl FileLogStore {
    /// The paths for one agent. Fails if the agent id can't be a single path component.
    fn paths(&self, agent: &AgentId) -> Result<Paths> {
        let dir = self.dir.join(path_component(agent.as_str())?);
        Ok(Paths {
            log: dir.join("log.jsonl"),
            epoch: dir.join("epoch"),
            dir,
        })
    }

    /// Reads the header and every complete event. A torn last line (a crash mid-write) is
    /// cut off so later appends start on a clean line.
    ///
    /// Fails with [`Error::AgentNotFound`] when there's no log file, and with a log or JSON
    /// error when the header is missing or unreadable. Reading stops at the first line that
    /// doesn't parse or lacks its newline, and the file is truncated there. A torn last
    /// line is the expected case, but a corrupt line in the middle also drops every event
    /// after it. The truncation happens before the caller raises the epoch, so it runs
    /// even when this open is about to fail as stale.
    async fn read(&self, paths: &Paths, agent: &AgentId) -> Result<(LogHeader, Vec<Event>)> {
        let text = match tokio::fs::read_to_string(&paths.log).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::AgentNotFound(agent.clone()));
            }
            Err(e) => return Err(e.into()),
        };
        let mut lines = text.split_inclusive('\n');
        let first = lines
            .next()
            .ok_or_else(|| Error::Log(format!("the log of `{agent}` is empty")))?;
        let header: HeaderLine = serde_json::from_str(first)?;
        let mut events = Vec::new();
        let mut good_len = first.len();
        for line in lines {
            match serde_json::from_str::<EventLine>(line) {
                Ok(e) if line.ends_with('\n') => {
                    events.push(e.event);
                    good_len += line.len();
                }
                _ => break,
            }
        }
        if good_len < text.len() {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&paths.log)
                .await?;
            file.set_len(good_len as u64).await?;
            file.sync_all().await?;
        }
        Ok((header.header, events))
    }

    /// Reads an epoch file. A missing file reads as 0, which any open then raises; a file
    /// that isn't a number is a log error.
    async fn read_epoch(path: &std::path::Path) -> Result<u64> {
        match tokio::fs::read_to_string(path).await {
            Ok(t) => t
                .trim()
                .parse()
                .map_err(|_| Error::Log(format!("{} is not an epoch", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl LogStore for FileLogStore {
    /// Opens the agent's log as its only writer, in the given mode.
    ///
    /// `Create` fails with [`Error::AgentExists`] if a log file is already there; otherwise
    /// it writes and syncs the header, then sets the epoch to 1. `ContinueFrom` fails with
    /// [`Error::StaleImage`] unless the log holds exactly as many events as the image's
    /// position, then sets the epoch to one more than the larger of the file's epoch and
    /// the image's. `Recover` sets it to one more than the file's and returns every event.
    /// The new epoch is written before this returns, which fences off any earlier owner.
    async fn open(&self, agent: &AgentId, mode: OpenMode) -> Result<OpenedLog> {
        let paths = self.paths(agent)?;
        let (header, events, epoch) = match mode {
            OpenMode::Create(header) => {
                tokio::fs::create_dir_all(&paths.dir).await?;
                let mut line = serde_json::to_vec(&HeaderLine {
                    header: header.clone(),
                })?;
                line.push(b'\n');
                let mut file = match tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&paths.log)
                    .await
                {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(Error::AgentExists(agent.clone()));
                    }
                    Err(e) => return Err(e.into()),
                };
                file.write_all(&line).await?;
                file.sync_all().await?;
                write_atomic(&paths.epoch, b"1").await?;
                (header, Vec::new(), 1)
            }
            OpenMode::ContinueFrom(position) => {
                let (header, events) = self.read(&paths, agent).await?;
                if events.len() as u64 != position.events {
                    return Err(Error::StaleImage {
                        image: position.events,
                        log: events.len() as u64,
                    });
                }
                let epoch = Self::read_epoch(&paths.epoch).await?.max(position.epoch) + 1;
                write_atomic(&paths.epoch, epoch.to_string().as_bytes()).await?;
                (header, events, epoch)
            }
            OpenMode::Recover => {
                let (header, events) = self.read(&paths, agent).await?;
                let epoch = Self::read_epoch(&paths.epoch).await? + 1;
                write_atomic(&paths.epoch, epoch.to_string().as_bytes()).await?;
                (header, events, epoch)
            }
        };
        let log = FileLog {
            agent: agent.clone(),
            log: paths.log,
            epoch_path: paths.epoch,
            epoch,
            events: events.len() as u64,
            buffer: Vec::new(),
        };
        Ok(OpenedLog {
            log: Box::new(log),
            header,
            events,
        })
    }

    /// Whether the agent's log file exists. Fails only for an id that can't be a file name
    /// or an I/O error.
    async fn exists(&self, agent: &AgentId) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.paths(agent)?.log).await?)
    }
}

/// One agent's open log, held by its current owner.
struct FileLog {
    /// The agent, for the [`Error::Fenced`] message.
    agent: AgentId,
    /// The log file appended to at each sync.
    log: PathBuf,
    /// The epoch file checked before each sync.
    epoch_path: PathBuf,
    /// The epoch this owner took at open.
    epoch: u64,
    /// Events in the log, counting buffered ones; this is [`LogPosition::events`].
    events: u64,
    /// Encoded lines appended since the last sync, newline-terminated.
    buffer: Vec<u8>,
}

#[async_trait]
impl SessionLog for FileLog {
    /// Where the log ends, counting buffered events, with this owner's epoch.
    fn position(&self) -> LogPosition {
        LogPosition {
            events: self.events,
            epoch: self.epoch,
        }
    }

    /// Encodes one event line into the buffer. Touches no file; fails only if the event
    /// can't be serialized.
    fn append(&mut self, event: &Event) -> Result<()> {
        serde_json::to_writer(
            &mut self.buffer,
            &EventLine {
                epoch: self.epoch,
                event: event.clone(),
            },
        )?;
        self.buffer.push(b'\n');
        self.events += 1;
        Ok(())
    }

    /// Writes the buffer to the end of the log file and syncs it. Returns at once when the
    /// buffer is empty, without checking the epoch.
    ///
    /// Fails with [`Error::Fenced`] when the epoch file no longer holds this owner's epoch.
    /// The buffered events are then discarded, but `events` keeps counting them, so the
    /// log's position no longer matches the file; the owner is expected to stop. An I/O
    /// error during the write keeps the buffer, so a later sync would write it again after
    /// whatever part of it reached the file.
    async fn sync(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let current = FileLogStore::read_epoch(&self.epoch_path).await?;
        if current != self.epoch {
            self.buffer.clear();
            return Err(Error::Fenced {
                agent: self.agent.clone(),
                ours: self.epoch,
                current,
            });
        }
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&self.log)
            .await?;
        file.write_all(&self.buffer).await?;
        file.sync_data().await?;
        self.buffer.clear();
        Ok(())
    }

    /// A final sync. There's no lease to give up; the next owner's open raises the epoch
    /// regardless.
    async fn release(&mut self) -> Result<()> {
        self.sync().await
    }
}
