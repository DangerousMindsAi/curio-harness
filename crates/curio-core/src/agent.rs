//! The agent context and the handle outside callers use to reach an agent.
//!
//! An agent is one task that owns its [`AgentCtx`]. Callers hold an [`AgentHandle`], which
//! sends [`Command`]s over a channel and publishes messages to the agent's queue; nothing
//! outside the task ever holds a reference into the context.

use crate::config::AgentConfig;
use crate::error::{Error, Result};
use crate::event::{Delivery, Event, TurnEnd};
use crate::hook::{HookEvent, dispatch};
use crate::ids::{AgentId, CallId, QueueMsgId, RequestId, TenantId, TurnId};
use crate::image::{AgentImage, IMAGE_FORMAT, SlotImage};
use crate::log::Journal;
use crate::plugin::{Plugin, PluginEntry, PluginState, PluginTable, ToolCx};
use crate::queue::QueueConsumer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Where a drain may stop the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainPoint {
    /// At the next step boundary. The turn is suspended and continues on thaw.
    Step,
    /// After the current turn ends.
    Turn,
    /// Cancel the current turn now, then quiesce.
    Immediate,
}

impl DrainPoint {
    /// Whether reaching `self` satisfies a drain requested at `requested`. Only `Step` and
    /// `Turn` are ever reached; a turn boundary satisfies every request.
    pub fn satisfies(self, requested: DrainPoint) -> bool {
        match self {
            DrainPoint::Turn => true,
            DrainPoint::Step => requested == DrainPoint::Step,
            DrainPoint::Immediate => false,
        }
    }
}

/// How the agent came to be loaded in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadKind {
    Created,
    Thawed,
    Recovered,
}

/// An answer to a question a plugin asked, such as a tool approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub approve: bool,
    pub note: Option<String>,
}

/// What a drain hands back.
#[derive(Debug)]
pub enum DrainResult {
    /// The agent stopped at a safe point; `forced` is set when the deadline cut a turn short.
    Quiesced {
        image: Box<AgentImage>,
        forced: bool,
    },
    /// The agent stopped on an error before it could quiesce; recover it from its log.
    Failed(String),
}

/// Requests to the agent task. Messages for the model go through the queue instead.
#[derive(Debug)]
pub enum Command {
    /// Cancel the current turn.
    Cancel,
    /// Stop at a safe point and hand back an image; force an immediate stop at `deadline`.
    Drain {
        at: DrainPoint,
        deadline: Instant,
        reply: oneshot::Sender<DrainResult>,
    },
    /// Answer a question a plugin asked.
    Answer { request: RequestId, answer: Answer },
}

/// How the driver's `run` ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Quiesced,
    Closed,
}

/// What the agent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Quiesced,
    Closed,
}

/// Live output for whoever watches the agent. Not logged: everything that matters for
/// the model is in the log already.
#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    TurnStarted {
        turn: TurnId,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCalled {
        call_id: CallId,
        name: String,
        arguments: String,
    },
    ToolFinished {
        call_id: CallId,
        name: String,
        content: String,
        is_error: bool,
    },
    ApprovalRequested {
        request: RequestId,
        tool: String,
        arguments: String,
        question: String,
    },
    Retrying {
        error: String,
    },
    TurnEnded {
        turn: TurnId,
        end: TurnEnd,
    },
    Quiesced {
        forced: bool,
    },
    Notice(String),
}

/// Sends [`Observation`]s to every subscriber. Sending never blocks or fails.
#[derive(Clone)]
pub struct Observer(broadcast::Sender<Observation>);

impl Observer {
    /// An observer with no subscribers yet.
    pub fn new() -> Self {
        Self(broadcast::channel(4096).0)
    }

    /// Delivers to current subscribers; dropped when there are none.
    pub fn emit(&self, observation: Observation) {
        let _ = self.0.send(observation);
    }

    /// A new subscription, starting with the next observation.
    pub fn subscribe(&self) -> broadcast::Receiver<Observation> {
        self.0.subscribe()
    }
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

struct DrainState {
    at: DrainPoint,
    deadline: Instant,
    forced: bool,
    waiters: Vec<oneshot::Sender<DrainResult>>,
}

/// All mutable state of one agent. Owned by the agent's task and passed by `&mut` to the
/// driver and, in narrower views, to every hook.
pub struct AgentCtx {
    pub id: AgentId,
    pub tenant: TenantId,
    pub host_id: String,
    pub config: Arc<AgentConfig>,
    pub plugins: Arc<PluginTable>,
    /// One state slot per plugin, parallel to `plugins.entries()`.
    pub state: Vec<Box<dyn PluginState>>,
    pub journal: Journal,
    pub queue: Option<Box<dyn QueueConsumer>>,
    pub phase: Phase,
    pub loaded_by: LoadKind,
    /// Cancelled to cancel the current turn. Replaced at the start of each turn.
    pub turn_cancel: CancellationToken,
    /// Answers received for questions plugins asked.
    pub answers: HashMap<RequestId, Answer>,
    pub observer: Observer,
    pub(crate) replaying: bool,
    drain: Option<DrainState>,
    drain_hook_pending: Option<DrainPoint>,
    commands: mpsc::Receiver<Command>,
    commands_closed: bool,
}

impl AgentCtx {
    pub(crate) fn new(
        config: AgentConfig,
        host_id: String,
        plugins: PluginTable,
        state: Vec<Box<dyn PluginState>>,
        journal: Journal,
        loaded_by: LoadKind,
    ) -> (Self, mpsc::Sender<Command>) {
        let (tx, commands) = mpsc::channel(64);
        let cx = Self {
            id: config.id.clone(),
            tenant: config.tenant.clone(),
            host_id,
            config: Arc::new(config),
            plugins: Arc::new(plugins),
            state,
            journal,
            queue: None,
            phase: Phase::Idle,
            loaded_by,
            turn_cancel: CancellationToken::new(),
            answers: HashMap::new(),
            observer: Observer::new(),
            replaying: false,
            drain: None,
            drain_hook_pending: None,
            commands,
            commands_closed: false,
        };
        (cx, tx)
    }

    /// Starts a fresh cancellation scope for a new turn.
    pub fn begin_turn_scope(&mut self) {
        self.turn_cancel = CancellationToken::new();
    }

    /// The point a pending drain asked for, if one is pending.
    pub fn draining(&self) -> Option<DrainPoint> {
        self.drain.as_ref().map(|d| d.at)
    }

    /// The tool context for one call, with the providing plugin's grants.
    pub fn tool_cx(&self, entry: &PluginEntry, call_id: CallId) -> ToolCx {
        ToolCx {
            agent: self.id.clone(),
            tenant: self.tenant.clone(),
            call_id,
            grants: entry.grants.clone(),
            cancel: self.turn_cancel.child_token(),
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Cancel => self.turn_cancel.cancel(),
            Command::Answer { request, answer } => {
                self.answers.insert(request, answer);
            }
            Command::Drain {
                at,
                deadline,
                reply,
            } => {
                match &mut self.drain {
                    Some(d) => {
                        d.waiters.push(reply);
                        d.deadline = d.deadline.min(deadline);
                        if at == DrainPoint::Immediate
                            || (at == DrainPoint::Step && d.at == DrainPoint::Turn)
                        {
                            d.at = at;
                        }
                    }
                    None => {
                        self.drain = Some(DrainState {
                            at,
                            deadline,
                            forced: false,
                            waiters: vec![reply],
                        });
                    }
                }
                if at == DrainPoint::Immediate {
                    self.turn_cancel.cancel();
                }
                self.drain_hook_pending = Some(at);
            }
        }
    }

    fn force_drain(&mut self) {
        if let Some(d) = &mut self.drain {
            d.forced = true;
            d.at = DrainPoint::Immediate;
            self.turn_cancel.cancel();
            self.observer.emit(Observation::Notice(
                "drain deadline passed; cancelling the turn".into(),
            ));
        }
    }

    /// Handles every command already waiting, without blocking.
    pub fn poll_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            self.handle_command(command);
        }
    }

    /// Polls `fut` until it completes, a command arrives, or a drain deadline passes.
    /// Returns `None` after handling a command or a deadline; the future can be polled again.
    pub async fn wait_one<F: Future>(&mut self, mut fut: Pin<&mut F>) -> Option<F::Output> {
        let deadline = self
            .drain
            .as_ref()
            .filter(|d| !d.forced)
            .map(|d| d.deadline);
        tokio::select! {
            biased;
            out = fut.as_mut() => Some(out),
            command = self.commands.recv(), if !self.commands_closed => {
                match command {
                    Some(command) => self.handle_command(command),
                    None => self.commands_closed = true,
                }
                None
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                self.force_drain();
                None
            }
        }
    }

    /// Runs `fut` to completion while handling commands and drain deadlines. `fut` must
    /// not borrow the context; it should watch `turn_cancel` to stop early.
    pub async fn wait<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        loop {
            if let Some(out) = self.wait_one(fut.as_mut()).await {
                return out;
            }
        }
    }

    /// Handles waiting commands and dispatches `drain_requested` for a new drain.
    pub async fn apply_pending_commands(&mut self) -> Result<()> {
        self.poll_commands();
        if let Some(at) = self.drain_hook_pending.take() {
            dispatch(self, &mut HookEvent::DrainRequested { at }).await?;
        }
        Ok(())
    }

    /// Pulls up to `max` messages, logs the new ones, syncs, then acknowledges them. With
    /// `wait`, waits for a message, but returns early (with 0) when a command arrives.
    /// Does nothing while a drain is pending, so new messages stay in the queue.
    pub async fn pull_messages(&mut self, max: usize, wait: bool) -> Result<usize> {
        if self.drain.is_some() {
            return Ok(0);
        }
        let mut queue = self
            .queue
            .take()
            .ok_or_else(|| Error::Queue("the agent is not attached to its queue".into()))?;
        let pulled = {
            let fut = queue.pull(max, wait);
            tokio::pin!(fut);
            if wait {
                self.wait_one(fut).await
            } else {
                Some(fut.await)
            }
        };
        self.queue = Some(queue);
        let batch = match pulled {
            None => return Ok(0),
            Some(batch) => batch?,
        };
        let Some(last) = batch.last().map(|m| m.id) else {
            return Ok(0);
        };
        let mut logged = 0;
        for message in batch {
            if self
                .journal
                .view()
                .inbox
                .last_logged
                .is_some_and(|l| message.id <= l)
            {
                continue; // delivered again after a crash; already in the log
            }
            self.journal.append(Event::MessageReceived { message })?;
            logged += 1;
        }
        self.journal.sync().await?;
        if let Some(queue) = self.queue.as_mut() {
            queue.ack(last).await?;
        }
        Ok(logged)
    }

    /// Quiesces if a pending drain is satisfied by reaching `here`: snapshots plugin state,
    /// logs `Quiesced`, syncs, builds the image, releases the log, detaches from the queue
    /// and hands the image to every drain waiter.
    pub async fn settle_drain(&mut self, here: DrainPoint) -> Result<Option<Exit>> {
        match &self.drain {
            Some(d) if here.satisfies(d.at) => {}
            _ => return Ok(None),
        }
        dispatch(self, &mut HookEvent::Snapshot).await?;
        let Some(drain) = self.drain.take() else {
            return Ok(None);
        };
        let suspended_turn = if here == DrainPoint::Step {
            self.journal.view().open_turn
        } else {
            None
        };
        self.journal.append(Event::Quiesced {
            at: here,
            suspended_turn,
            forced: drain.forced,
        })?;
        self.journal.sync().await?;
        let image = self.to_image()?;
        self.journal.release().await?;
        if let Some(queue) = self.queue.take() {
            queue.detach().await?;
        }
        self.phase = Phase::Quiesced;
        self.observer.emit(Observation::Quiesced {
            forced: drain.forced,
        });
        for waiter in drain.waiters {
            let _ = waiter.send(DrainResult::Quiesced {
                image: Box::new(image.clone()),
                forced: drain.forced,
            });
        }
        Ok(Some(Exit::Quiesced))
    }

    /// Tells every drain waiter the agent stopped without an image.
    pub fn fail_drain_waiters(&mut self, reason: &str) {
        self.poll_commands();
        if let Some(drain) = self.drain.take() {
            for waiter in drain.waiters {
                let _ = waiter.send(DrainResult::Failed(reason.to_string()));
            }
        }
    }

    /// The whole context as data: config, every slot, the view and the log position.
    pub fn to_image(&self) -> Result<AgentImage> {
        let slots = self
            .plugins
            .entries()
            .iter()
            .zip(&self.state)
            .map(|(entry, state)| {
                Ok(SlotImage {
                    plugin: entry.id.clone(),
                    version: entry.state_version,
                    blob: state.to_blob()?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(AgentImage {
            format: IMAGE_FORMAT,
            config: (*self.config).clone(),
            slots,
            view: self.journal.view().clone(),
            log_position: self.journal.position(),
        })
    }
}

/// How callers reach a running agent. Cheap to clone; holds no agent state.
#[derive(Clone)]
pub struct AgentHandle {
    pub id: AgentId,
    pub tenant: TenantId,
    commands: mpsc::Sender<Command>,
    observer: Observer,
    queue: Arc<dyn Plugin>,
}

impl AgentHandle {
    pub(crate) fn new(cx: &AgentCtx, commands: mpsc::Sender<Command>) -> Self {
        Self {
            id: cx.id.clone(),
            tenant: cx.tenant.clone(),
            commands,
            observer: cx.observer.clone(),
            queue: cx.plugins.queue().clone(),
        }
    }

    /// Publishes a message to the agent's queue.
    pub async fn send(&self, delivery: Delivery, text: impl Into<String>) -> Result<QueueMsgId> {
        let queue = self
            .queue
            .as_queue()
            .ok_or_else(|| Error::Queue("queue plugin has no queue service".into()))?;
        queue.publish(&self.id, delivery, text.into()).await
    }

    /// Subscribes to live observations from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Observation> {
        self.observer.subscribe()
    }

    /// Cancels the current turn.
    pub async fn cancel(&self) -> Result<()> {
        self.commands
            .send(Command::Cancel)
            .await
            .map_err(|_| Error::AgentGone)
    }

    /// Answers a question a plugin asked.
    pub async fn answer(&self, request: RequestId, answer: Answer) -> Result<()> {
        self.commands
            .send(Command::Answer { request, answer })
            .await
            .map_err(|_| Error::AgentGone)
    }

    /// Drains the agent at `at`, forcing an immediate stop after `timeout`, and returns its image.
    pub async fn drain(&self, at: DrainPoint, timeout: Duration) -> Result<DrainResult> {
        let (reply, rx) = oneshot::channel();
        let deadline = Instant::now() + timeout;
        self.commands
            .send(Command::Drain {
                at,
                deadline,
                reply,
            })
            .await
            .map_err(|_| Error::AgentGone)?;
        rx.await.map_err(|_| Error::AgentGone)
    }
}

/// A spawned agent: its handle and the task that runs its driver.
pub struct RunningAgent {
    pub handle: AgentHandle,
    pub task: JoinHandle<Result<Exit>>,
}
