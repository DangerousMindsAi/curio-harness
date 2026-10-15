//! `builtin.queue.file`: a message queue kept in files, one directory per agent. It stands
//! in for a shared broker on a single machine: messages survive a restart, so anything sent
//! while an agent is quiesced is waiting when it is thawed.
//!
//! Layout under `dir`: `<agent>/messages.jsonl` (one message per line, ids increasing) and
//! `<agent>/acked` (the newest acknowledged id).

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
#[derive(Default)]
pub struct FileQueueRunner {
    brokers: Mutex<HashMap<PathBuf, Arc<Broker>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueCode {
    dir: PathBuf,
    /// How often a waiting consumer rereads its file, for messages published by other processes.
    #[serde(default = "default_poll")]
    poll_ms: u64,
}

fn default_poll() -> u64 {
    250
}

/// The shared state of one queue directory: publish serialization, wakeups and the set of
/// attached consumers. Holds no messages; those are in the files.
struct Broker {
    dir: PathBuf,
    publish: tokio::sync::Mutex<()>,
    wakers: Mutex<HashMap<AgentId, Arc<Notify>>>,
    attached: Mutex<HashSet<AgentId>>,
}

impl Broker {
    fn agent_dir(&self, agent: &AgentId) -> Result<PathBuf> {
        Ok(self.dir.join(path_component(agent.as_str())?))
    }

    fn waker(&self, agent: &AgentId) -> Arc<Notify> {
        self.wakers
            .lock()
            .expect("waker map")
            .entry(agent.clone())
            .or_default()
            .clone()
    }
}

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

/// One agent's view of a queue directory.
pub struct FileQueue {
    broker: Arc<Broker>,
    poll: Duration,
}

impl Plugin for FileQueue {
    fn as_queue(&self) -> Option<&dyn MessageQueue> {
        Some(self)
    }
}

#[async_trait]
impl MessageQueue for FileQueue {
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

struct FileConsumer {
    broker: Arc<Broker>,
    agent: AgentId,
    dir: PathBuf,
    cursor: Option<QueueMsgId>,
    poll: Duration,
}

#[async_trait]
impl QueueConsumer for FileConsumer {
    async fn pull(&mut self, max: usize, wait: bool) -> Result<Vec<QueuedMessage>> {
        let waker = self.broker.waker(&self.agent);
        loop {
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

    async fn detach(self: Box<Self>) -> Result<()> {
        self.broker
            .attached
            .lock()
            .expect("attached set")
            .remove(&self.agent);
        Ok(())
    }
}

impl Drop for FileConsumer {
    fn drop(&mut self) {
        if let Ok(mut attached) = self.broker.attached.lock() {
            attached.remove(&self.agent);
        }
    }
}
