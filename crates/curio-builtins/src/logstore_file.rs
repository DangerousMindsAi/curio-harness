//! `builtin.logstore.file`: session logs as JSON Lines files on local disk, with an epoch
//! file per agent for fencing.
//!
//! Layout under `dir`: `<agent>/log.jsonl` (the header on line 1, then one event per line)
//! and `<agent>/epoch` (the current owner's epoch). Every open except `Create` raises the
//! epoch; a sync from an older epoch fails. The check and the write are two steps, which is
//! enough for one machine; a shared store would make them one conditional write.

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

/// Loads [`FileLogStore`].
pub struct FileLogStoreRunner;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreCode {
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

/// Session logs in a directory.
pub struct FileLogStore {
    dir: PathBuf,
}

impl Plugin for FileLogStore {
    fn as_log_store(&self) -> Option<&dyn LogStore> {
        Some(self)
    }
}

#[derive(Serialize, Deserialize)]
struct HeaderLine {
    header: LogHeader,
}

#[derive(Serialize, Deserialize)]
struct EventLine {
    epoch: u64,
    event: Event,
}

struct Paths {
    dir: PathBuf,
    log: PathBuf,
    epoch: PathBuf,
}

impl FileLogStore {
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

    async fn exists(&self, agent: &AgentId) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.paths(agent)?.log).await?)
    }
}

struct FileLog {
    agent: AgentId,
    log: PathBuf,
    epoch_path: PathBuf,
    epoch: u64,
    events: u64,
    buffer: Vec<u8>,
}

#[async_trait]
impl SessionLog for FileLog {
    fn position(&self) -> LogPosition {
        LogPosition {
            events: self.events,
            epoch: self.epoch,
        }
    }

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

    async fn release(&mut self) -> Result<()> {
        self.sync().await
    }
}
