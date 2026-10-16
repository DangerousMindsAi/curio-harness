//! `builtin.queue.file`: a message queue kept in files, one directory per agent.
//!
//! It stands in for a shared broker on a single machine: messages survive a restart, so
//! anything sent while an agent is quiesced or stopped is waiting when it's thawed or
//! recovered. Every agent needs exactly one queue plugin. It's per-agent and implements
//! [`MessageQueue`]: [`curio_core::Runtime`] attaches a [`QueueConsumer`] when it starts the
//! agent, the driver pulls through [`curio_core::AgentCtx::pull_messages`], and callers
//! publish through [`curio_core::AgentHandle::send`]. It subscribes to no hooks and has no
//! state slot; its state is the files.
//!
//! ```yaml
//! - id: queue
//!   runner: builtin.queue.file
//!   code: { dir: data/queues, poll_ms: 250 }   # poll_ms is optional
//! ```
//!
//! Layout under `dir`: `<agent>/messages.jsonl` (one message per line, ids increasing) and
//! `<agent>/acked` (the newest acknowledged id). A message moves through three stages. A
//! publish appends it with the next id and wakes the consumer. A pull returns it and
//! advances the consumer's in-memory cursor, without touching the files. Once the agent
//! has logged it and synced its log, an ack records its id in `acked` and rewrites
//! `messages.jsonl` without it. A consumer that's dropped before acking loses only its
//! cursor, so the next consumer, which starts from `acked`, gets those messages again;
//! the agent skips ones it already logged.
//!
//! Coordination is per process. Agents that name the same directory through one runner
//! share a `Broker`, which serializes publishes and acks, refuses a second consumer for
//! the same agent, and wakes a waiting consumer at once. Another process sharing the
//! directory gets none of that: its consumer only sees new messages on its next poll, two
//! processes could both attach as consumers, and publishes from two processes aren't
//! serialized, so they can pick the same id. Use one process per queue directory, or a
//! real broker.

use crate::fsutil::{path_component, write_atomic};
use async_trait::async_trait;
use curio_core::{
    AgentId, Delivery, Error, LoadEnv, LoadedPlugin, MessageQueue, Plugin, QueueConsumer,
    QueueMsgId, QueuedMessage, Result, Runner,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

/// Loads [`FileQueue`]s. Agents whose code names the same directory share one broker.
///
/// The broker map is runner-level state shared by every agent. It holds only tenant-neutral
/// coordination (locks and wakeups keyed by directory), never message contents, and its
/// entries are never removed for the life of the runner.
#[derive(Default)]
pub struct FileQueueRunner {
    /// One broker per canonical queue directory.
    brokers: Mutex<HashMap<PathBuf, Arc<Broker>>>,
}

/// The queue plugin's code.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueCode {
    /// The queue directory, shared by every agent that names it. Required; relative paths
    /// resolve against the startup file's directory, and the directory is created if missing.
    dir: PathBuf,
    /// How often a waiting consumer rereads its file, for messages published by other
    /// processes, in milliseconds. Defaults to 250. Publishes from this process wake the
    /// consumer at once and don't wait for the poll.
    #[serde(default = "default_poll")]
    poll_ms: u64,
}

/// Default for [`QueueCode::poll_ms`].
fn default_poll() -> u64 {
    250
}

/// The shared state of one queue directory: publish serialization, wakeups and the set of
/// attached consumers. Holds no messages; those are in the files.
struct Broker {
    /// The canonical queue directory.
    dir: PathBuf,
    /// Held while a publish picks an id and appends, and while an ack rewrites the message
    /// file, so neither sees the other half done. Covers this process only.
    publish: tokio::sync::Mutex<()>,
    /// One wakeup per agent, created on first use and never removed. A publish stores a
    /// permit when no consumer is waiting, so a publish that lands between a pull's read
    /// and its wait isn't missed.
    wakers: Mutex<HashMap<AgentId, Arc<Notify>>>,
    /// Agents with a live consumer in this process.
    attached: Mutex<HashSet<AgentId>>,
}

impl Broker {
    /// The agent's directory under the queue directory. Fails if the agent id can't be a
    /// single path component.
    fn agent_dir(&self, agent: &AgentId) -> Result<PathBuf> {
        Ok(self.dir.join(path_component(agent.as_str())?))
    }

    /// The agent's wakeup, created if this is the first time anyone asked.
    fn waker(&self, agent: &AgentId) -> Arc<Notify> {
        self.wakers
            .lock()
            .expect("waker map")
            .entry(agent.clone())
            .or_default()
            .clone()
    }
}

/// Reads every message in a message file, oldest first. A missing file is an empty queue.
///
/// Lines that don't parse are skipped. That covers a publish still being written, but it
/// also means a line torn by a crash is dropped for good, and if a later publish appends
/// to the end of that torn line, the new message is lost with it.
async fn read_messages(path: &Path) -> Result<Vec<QueuedMessage>> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    // A torn last line is a publish still in progress; it is read on a later pull.
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// Reads the newest acknowledged id. `None` when nothing has been acknowledged yet; fails
/// when the file holds something other than a number.
async fn read_acked(path: &Path) -> Result<Option<QueueMsgId>> {
    match tokio::fs::read_to_string(path).await {
        Ok(t) => Ok(Some(QueueMsgId(t.trim().parse().map_err(|_| {
            Error::Queue(format!("{} is not a message id", path.display()))
        })?))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[async_trait]
impl Runner for FileQueueRunner {
    fn name(&self) -> &str {
        "builtin.queue.file"
    }

    /// Creates and canonicalizes the queue directory, then finds or creates its broker, so
    /// two spellings of the same directory share one.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: QueueCode = env.parse_code(code)?;
        let dir = env.resolve_path(&code.dir);
        tokio::fs::create_dir_all(&dir).await?;
        let dir = tokio::fs::canonicalize(&dir).await?;
        let broker = self
            .brokers
            .lock()
            .expect("broker map")
            .entry(dir.clone())
            .or_insert_with(|| {
                Arc::new(Broker {
                    dir,
                    publish: Default::default(),
                    wakers: Default::default(),
                    attached: Default::default(),
                })
            })
            .clone();
        Ok(LoadedPlugin::stateless(Arc::new(FileQueue {
            broker,
            poll: Duration::from_millis(code.poll_ms),
        })))
    }
}

/// One agent's handle on a queue directory. Immutable; the agent it serves is named in
/// each call, since [`MessageQueue`] methods take the agent id.
pub struct FileQueue {
    /// The directory's shared broker.
    broker: Arc<Broker>,
    /// From [`QueueCode::poll_ms`].
    poll: Duration,
}

impl Plugin for FileQueue {
    fn as_queue(&self) -> Option<&dyn MessageQueue> {
        Some(self)
    }
}

#[async_trait]
impl MessageQueue for FileQueue {
    /// Registers the only consumer of the agent's queue in this process, starting just
    /// after the newest acknowledged message. Fails while another consumer in this process
    /// is attached; a consumer in another process isn't detected.
    async fn attach(&self, agent: &AgentId) -> Result<Box<dyn QueueConsumer>> {
        let dir = self.broker.agent_dir(agent)?;
        tokio::fs::create_dir_all(&dir).await?;
        if !self
            .broker
            .attached
            .lock()
            .expect("attached set")
            .insert(agent.clone())
        {
            return Err(Error::Queue(format!(
                "agent `{agent}` already has a consumer in this process"
            )));
        }
        let cursor = read_acked(&dir.join("acked")).await?;
        Ok(Box::new(FileConsumer {
            broker: self.broker.clone(),
            agent: agent.clone(),
            dir,
            cursor,
            poll: self.poll,
        }))
    }

    /// Appends a message with the next id and wakes the agent's consumer if it's waiting.
    /// Works whether or not the agent is running or has ever been created.
    ///
    /// The id is one more than the larger of the last message in the file and the last
    /// acknowledged id, so ids keep increasing after an ack empties the file. The append is
    /// synced before this returns, so a returned id survives a crash.
    async fn publish(
        &self,
        agent: &AgentId,
        delivery: Delivery,
        body: String,
    ) -> Result<QueueMsgId> {
        let dir = self.broker.agent_dir(agent)?;
        let _guard = self.broker.publish.lock().await;
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join("messages.jsonl");
        let last = read_messages(&path).await?.last().map(|m| m.id.0);
        let acked = read_acked(&dir.join("acked")).await?.map(|a| a.0);
        let id = QueueMsgId(last.max(acked).unwrap_or(0) + 1);
        let message = QueuedMessage {
            id,
            sent_at_ms: curio_core::now_ms(),
            delivery,
            body,
        };
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(&line).await?;
        file.sync_data().await?;
        self.broker.waker(agent).notify_one();
        Ok(id)
    }
}

/// The attached consumer of one agent's queue.
struct FileConsumer {
    /// The directory's shared broker, for wakeups, the publish lock and detaching.
    broker: Arc<Broker>,
    /// The agent this consumer serves.
    agent: AgentId,
    /// The agent's directory under the queue directory.
    dir: PathBuf,
    /// The newest id this consumer has returned from a pull; `None` before the first
    /// message. In memory only, so a new consumer starts again from `acked`.
    cursor: Option<QueueMsgId>,
    /// How long a waiting pull sleeps between rereads.
    poll: Duration,
}

#[async_trait]
impl QueueConsumer for FileConsumer {
    /// Returns up to `max` messages after the cursor, oldest first, and moves the cursor
    /// past them. With `wait`, rereads the file whenever a publish wakes it or every
    /// `poll_ms` until something arrives.
    ///
    /// Cancel-safe: the cursor moves only in the same step that returns the batch, so
    /// dropping the future at any await point loses nothing.
    async fn pull(&mut self, max: usize, wait: bool) -> Result<Vec<QueuedMessage>> {
        let waker = self.broker.waker(&self.agent);
        loop {
            // Register for the wakeup before reading, so a publish between the read and the
            // select below still wakes this pull.
            let notified = waker.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut fresh: Vec<_> = read_messages(&self.dir.join("messages.jsonl"))
                .await?
                .into_iter()
                .filter(|m| self.cursor.is_none_or(|c| m.id > c))
                .collect();
            fresh.truncate(max);
            if let Some(last) = fresh.last() {
                self.cursor = Some(last.id);
                return Ok(fresh);
            }
            if !wait {
                return Ok(fresh);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(self.poll) => {}
            }
        }
    }

    /// Records `upto` as acknowledged, then rewrites the message file without anything at
    /// or below it.
    ///
    /// A crash between the two steps leaves acknowledged lines in the file, which is safe
    /// because consumers start after `acked`. The rewrite reads and replaces the whole
    /// file under the in-process publish lock; a message another process appends in that
    /// window is overwritten and lost.
    async fn ack(&mut self, upto: QueueMsgId) -> Result<()> {
        write_atomic(&self.dir.join("acked"), upto.0.to_string().as_bytes()).await?;
        // Drop acknowledged messages so the file does not grow without bound.
        let _guard = self.broker.publish.lock().await;
        let path = self.dir.join("messages.jsonl");
        let kept: Vec<_> = read_messages(&path)
            .await?
            .into_iter()
            .filter(|m| m.id > upto)
            .collect();
        let mut text = Vec::new();
        for m in &kept {
            text.extend(serde_json::to_vec(m)?);
            text.push(b'\n');
        }
        write_atomic(&path, &text).await
    }

    /// Frees the agent's consumer slot so it can be attached again. Unacknowledged
    /// messages stay in the file for the next consumer.
    async fn detach(self: Box<Self>) -> Result<()> {
        self.broker
            .attached
            .lock()
            .expect("attached set")
            .remove(&self.agent);
        Ok(())
    }
}

/// Frees the consumer slot even when the consumer is dropped without [`QueueConsumer::detach`],
/// as when an agent's task is aborted, so the agent can be recovered in the same process.
impl Drop for FileConsumer {
    fn drop(&mut self) {
        if let Ok(mut attached) = self.broker.attached.lock() {
            attached.remove(&self.agent);
        }
    }
}
