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
//! which the next open cuts off once it holds the epoch. A bad line anywhere else is
//! corruption and fails the open. A write that fails partway poisons the open log, so the
//! owner stops instead of writing after a partial line.
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

/// What [`FileLogStore::read`] found in a log file.
struct ReadLog {
    /// The header line.
    header: LogHeader,
    /// Every complete event, in order.
    events: Vec<Event>,
    /// When the last line is torn, the file length up to where it starts.
    torn: Option<u64>,
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

    /// Reads the header and every complete event, without changing the file.
    ///
    /// Only the last line may be bad: one that lacks its newline or doesn't parse is a
    /// torn write from a crash, and is reported in [`ReadLog::torn`] so the opener can cut
    /// it off after it has taken the epoch. A bad line with anything after it is
    /// corruption, not a torn write, and fails with [`Error::Log`] rather than dropping the
    /// events that follow it. Fails with [`Error::AgentNotFound`] when there's no log file,
    /// and with a log or JSON error when the header is missing or unreadable.
    async fn read(&self, paths: &Paths, agent: &AgentId) -> Result<ReadLog> {
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
        let mut torn = None;
        for (number, line) in lines.enumerate() {
            if torn.is_some() {
                return Err(Error::Log(format!(
                    "line {} of the log of `{agent}` is corrupt and is not the last line",
                    number + 1
                )));
            }
            match serde_json::from_str::<EventLine>(line) {
                Ok(e) if line.ends_with('\n') => {
                    events.push(e.event);
                    good_len += line.len();
                }
                _ => torn = Some(good_len as u64),
            }
        }
        Ok(ReadLog {
            header: header.header,
            events,
            torn,
        })
    }

    /// Cuts a torn last line off the log file, so later appends start on a clean line.
    /// Called only after the opener has written its new epoch.
    async fn truncate(paths: &Paths, len: u64) -> Result<()> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&paths.log)
            .await?;
        file.set_len(len).await?;
        file.sync_all().await?;
        Ok(())
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
    /// The new epoch is written before this returns, which fences off any earlier owner,
    /// and a torn last line is cut off only after that, so a previous owner that is still
    /// running cannot write past the cut.
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
                let read = self.read(&paths, agent).await?;
                if read.events.len() as u64 != position.events {
                    return Err(Error::StaleImage {
                        image: position.events,
                        log: read.events.len() as u64,
                    });
                }
                let epoch = Self::read_epoch(&paths.epoch).await?.max(position.epoch) + 1;
                write_atomic(&paths.epoch, epoch.to_string().as_bytes()).await?;
                if let Some(len) = read.torn {
                    Self::truncate(&paths, len).await?;
                }
                (read.header, read.events, epoch)
            }
            OpenMode::Recover => {
                let read = self.read(&paths, agent).await?;
                let epoch = Self::read_epoch(&paths.epoch).await? + 1;
                write_atomic(&paths.epoch, epoch.to_string().as_bytes()).await?;
                if let Some(len) = read.torn {
                    Self::truncate(&paths, len).await?;
                }
                (read.header, read.events, epoch)
            }
        };
        let log = FileLog {
            agent: agent.clone(),
            log: paths.log,
            epoch_path: paths.epoch,
            epoch,
            events: events.len() as u64,
            buffer: Vec::new(),
            buffered: 0,
            poisoned: None,
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
    /// How many events `buffer` holds.
    buffered: u64,
    /// Set when a write failed partway, so the file may end in part of the buffer. Every
    /// later sync fails with this message; the agent must be recovered from the file.
    poisoned: Option<String>,
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
        self.buffered += 1;
        Ok(())
    }

    /// Writes the buffer to the end of the log file and syncs it. Returns at once when the
    /// buffer is empty, without checking the epoch.
    ///
    /// Fails with [`Error::Fenced`] when the epoch file no longer holds this owner's epoch.
    /// The buffered events are then discarded and the position goes back to what the file
    /// holds. An I/O error while writing poisons the log: part of the buffer may be in the
    /// file, so this and every later sync fails with [`Error::Log`], and the agent has to be
    /// recovered from the file by a new owner.
    async fn sync(&mut self) -> Result<()> {
        if let Some(reason) = &self.poisoned {
            return Err(Error::Log(reason.clone()));
        }
        if self.buffer.is_empty() {
            return Ok(());
        }
        let current = FileLogStore::read_epoch(&self.epoch_path).await?;
        if current != self.epoch {
            self.buffer.clear();
            self.events -= std::mem::take(&mut self.buffered);
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
        let written = match file.write_all(&self.buffer).await {
            Ok(()) => file.sync_data().await,
            Err(e) => Err(e),
        };
        if let Err(e) = written {
            let reason = format!(
                "a write to the log of `{}` failed partway ({e}); recover the agent from its log",
                self.agent
            );
            self.poisoned = Some(reason.clone());
            return Err(Error::Log(reason));
        }
        self.buffer.clear();
        self.buffered = 0;
        Ok(())
    }

    /// A final sync. There's no lease to give up; the next owner's open raises the epoch
    /// regardless.
    async fn release(&mut self) -> Result<()> {
        self.sync().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use curio_core::{AgentConfig, LogHeader, ResumeConfig, TenantId};

    fn store(dir: &std::path::Path) -> FileLogStore {
        FileLogStore {
            dir: dir.to_path_buf(),
        }
    }

    fn header(id: &AgentId) -> LogHeader {
        LogHeader {
            format: 1,
            config: AgentConfig {
                id: id.clone(),
                tenant: TenantId::new("t"),
                resume: ResumeConfig::Wait,
                plugins: Vec::new(),
            },
            state_versions: Default::default(),
            created_at_ms: 0,
        }
    }

    async fn create_with_events(store: &FileLogStore, id: &AgentId, n: usize) {
        let mut opened = store.open(id, OpenMode::Create(header(id))).await.unwrap();
        for _ in 0..n {
            opened
                .log
                .append(&Event::AttemptFailed { error: "x".into() })
                .unwrap();
        }
        opened.log.sync().await.unwrap();
    }

    #[tokio::test]
    async fn torn_last_line_is_cut_after_taking_the_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = AgentId::new("a");
        create_with_events(&store, &id, 2).await;
        let paths = store.paths(&id).unwrap();
        let before = std::fs::read(&paths.log).unwrap();
        let mut torn = before.clone();
        torn.extend_from_slice(b"{\"epoch\":1,\"ev");
        std::fs::write(&paths.log, &torn).unwrap();

        let opened = store.open(&id, OpenMode::Recover).await.unwrap();
        assert_eq!(opened.events.len(), 2);
        assert_eq!(std::fs::read(&paths.log).unwrap(), before);
        assert_eq!(std::fs::read_to_string(&paths.epoch).unwrap(), "2");
    }

    #[tokio::test]
    async fn corrupt_line_before_the_end_fails_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = AgentId::new("a");
        create_with_events(&store, &id, 2).await;
        let paths = store.paths(&id).unwrap();
        let text = std::fs::read_to_string(&paths.log).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "not json";
        std::fs::write(&paths.log, lines.join("\n") + "\n").unwrap();

        let err = store.open(&id, OpenMode::Recover).await.err().unwrap();
        assert!(matches!(err, Error::Log(_)), "{err}");
        assert_eq!(
            std::fs::read_to_string(&paths.log).unwrap(),
            lines.join("\n") + "\n"
        );
    }

    #[tokio::test]
    async fn fenced_sync_rolls_the_position_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = AgentId::new("a");
        let mut first = store
            .open(&id, OpenMode::Create(header(&id)))
            .await
            .unwrap();
        let _second = store.open(&id, OpenMode::Recover).await.unwrap();
        first
            .log
            .append(&Event::AttemptFailed { error: "x".into() })
            .unwrap();
        assert_eq!(first.log.position().events, 1);
        assert!(matches!(first.log.sync().await, Err(Error::Fenced { .. })));
        assert_eq!(first.log.position().events, 0);
    }
}
