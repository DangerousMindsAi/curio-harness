//! The agent context and the handle outside callers use to reach an agent.
//!
//! An agent is one task that owns its [`AgentCtx`]. Callers hold an [`AgentHandle`], which
//! sends [`Command`]s over a channel and publishes messages to the agent's queue; nothing
//! outside the task ever holds a reference into the context.
//!
//! [`crate::Runtime`] builds the context (on create, thaw or recovery), attaches it to the
//! agent's queue, and spawns a task that passes it by `&mut` to the agent's
//! [`crate::Driver`]. The driver then owns the agent's control flow. The context gives it
//! the pieces every driver needs: the [`Journal`] that pairs the session log with the view
//! folded from it, the plugin table and state slots, the queue consumer, the current turn's
//! cancellation token, and the bookkeeping for commands and drains. Hooks see a narrower
//! [`crate::HookCx`] built from it, and tool bodies get a [`ToolCx`] that holds no
//! reference into it at all, so they can run concurrently while the driver keeps `&mut`.
//!
//! Commands are handled only where the driver lets them in: in
//! [`AgentCtx::apply_pending_commands`] at safe points, and while the driver waits on
//! something slow through [`AgentCtx::wait`] or [`AgentCtx::wait_one`]. A cancel cancels
//! the turn's token at once; a drain is recorded and settled by
//! [`AgentCtx::settle_drain`] when the driver reaches a step or turn boundary that
//! satisfies it. If the drain's deadline passes first, the waiting helpers force it: the
//! turn is cancelled and the agent quiesces at the next turn boundary.
//!
//! Live output (streamed text, tool activity, approvals) goes to an [`Observer`], a
//! broadcast channel that isn't logged. Everything the model sees is in the log already, so
//! observations are only for whoever is watching.

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

/// Where a drain may stop the agent. Also names the boundary the driver has reached when it
/// calls [`AgentCtx::settle_drain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainPoint {
    /// At the next step boundary. The turn is suspended and continues on thaw.
    Step,
    /// After the current turn ends.
    Turn,
    /// Cancel the current turn now, then quiesce at the turn boundary that follows.
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

/// How the agent came to be loaded in this process. Passed to `agent_loaded` listeners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadKind {
    /// [`crate::Runtime::create`] made a new agent.
    Created,
    /// [`crate::Runtime::thaw`] restored it from an image.
    Thawed,
    /// [`crate::Runtime::recover`] rebuilt it from its log after a crash.
    Recovered,
}

/// An answer to a question a plugin asked, such as a tool approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Whether the asked-about action may go ahead.
    pub approve: bool,
    /// Free text from the answerer; on a denial, the driver passes it on to the model.
    pub note: Option<String>,
}

/// What a drain hands back. Every waiter of the same drain gets the same result.
#[derive(Debug)]
pub enum DrainResult {
    /// The agent stopped at a safe point; `forced` is set when the deadline cut a turn short.
    Quiesced {
        /// The agent's image, consistent with the end of its log; thaw it to continue.
        image: Box<AgentImage>,
        /// Whether the deadline passed and the turn was cancelled to get here.
        forced: bool,
    },
    /// The agent stopped on an error before it could quiesce; recover it from its log.
    Failed(String),
}

/// Requests to the agent task. Messages for the model go through the queue instead.
///
/// Sent by an [`AgentHandle`] and handled by the context whenever the driver applies or
/// waits for commands. Commands aren't logged; what they cause is.
#[derive(Debug)]
pub enum Command {
    /// Cancel the current turn. Has no effect while the agent is idle.
    Cancel,
    /// Stop at a safe point and hand back an image; force an immediate stop at `deadline`.
    ///
    /// A drain that arrives while another is pending joins it: the reply is added to its
    /// waiters, the earlier deadline wins, and the stricter drain point wins (`Immediate`
    /// over `Step` over `Turn`).
    Drain {
        /// The safe point to stop at.
        at: DrainPoint,
        /// When to give up waiting and cancel the turn.
        deadline: Instant,
        /// Where the result goes.
        reply: oneshot::Sender<DrainResult>,
    },
    /// Answer a question a plugin asked. Stored in [`AgentCtx::answers`] until the asker
    /// takes it or the turn ends.
    Answer {
        /// The question being answered.
        request: RequestId,
        /// The answer.
        answer: Answer,
    },
}

/// How the driver's `run` ended without an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The agent quiesced and its image went to the drain waiters.
    Quiesced,
}

/// What the agent is doing. Set by the driver and by [`AgentCtx::settle_drain`]; for
/// display and checks, not persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for a message or a command between turns.
    Idle,
    /// Running a turn.
    Running,
    /// Quiesced; the task is about to exit.
    Quiesced,
}

/// Live output for whoever watches the agent. Not logged: everything that matters for
/// the model is in the log already.
#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    /// A turn started, or a suspended turn resumed after thaw.
    TurnStarted {
        /// The turn.
        turn: TurnId,
    },
    /// A piece of reply text, streamed by the model provider.
    TextDelta(String),
    /// A piece of reasoning text, streamed by the model provider.
    ReasoningDelta(String),
    /// A tool body is about to run (its `ToolStarted` is already durable).
    ToolCalled {
        /// The call.
        call_id: CallId,
        /// The tool's name.
        name: String,
        /// The arguments as JSON text.
        arguments: String,
    },
    /// A tool result was committed.
    ToolFinished {
        /// The call.
        call_id: CallId,
        /// The tool's name.
        name: String,
        /// The result text.
        content: String,
        /// Whether the call failed or didn't run.
        is_error: bool,
    },
    /// A plugin asked for approval; answer with [`AgentHandle::answer`] under `request`.
    ApprovalRequested {
        /// The id to answer under.
        request: RequestId,
        /// The tool being asked about.
        tool: String,
        /// The call's arguments as JSON text.
        arguments: String,
        /// The question.
        question: String,
    },
    /// A model attempt failed and will be retried.
    Retrying {
        /// The attempt's error, as text.
        error: String,
    },
    /// A turn ended and its end is logged.
    TurnEnded {
        /// The turn.
        turn: TurnId,
        /// How it ended.
        end: TurnEnd,
    },
    /// The agent quiesced.
    Quiesced {
        /// Whether the drain deadline forced it.
        forced: bool,
    },
    /// Anything else worth telling a watcher, such as a forced drain or the task stopping
    /// on an error.
    Notice(String),
}

/// Sends [`Observation`]s to every subscriber. Sending never blocks or fails.
///
/// Cloned into the agent's handle and into model providers, which stream deltas through it.
/// It's a bounded broadcast channel (4096 observations): a subscriber that falls further
/// behind loses the oldest ones and gets a `Lagged` error on its next receive.
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

/// A drain that's been requested and not yet settled.
struct DrainState {
    /// The strictest drain point requested so far.
    at: DrainPoint,
    /// The earliest deadline requested so far.
    deadline: Instant,
    /// Set once the deadline passed and the turn was cancelled.
    forced: bool,
    /// Everyone who asked for this drain; each gets the result.
    waiters: Vec<oneshot::Sender<DrainResult>>,
}

/// All mutable state of one agent. Owned by the agent's task and passed by `&mut` to the
/// driver and, in narrower views, to every hook.
///
/// Nothing in it is shared with another agent. `config` and `plugins` sit behind `Arc` so
/// code can hold them while the context is borrowed mutably, as [`crate::dispatch`] does
/// with the plugin table. The
/// context isn't serialized directly; [`AgentCtx::to_image`] takes the parts that are data,
/// and the runtime rebuilds the rest from the config.
pub struct AgentCtx {
    /// The agent's id.
    pub id: AgentId,
    /// The agent's tenant.
    pub tenant: TenantId,
    /// The host running the agent, from the runtime config.
    pub host_id: String,
    /// The agent's pinned configuration.
    pub config: Arc<AgentConfig>,
    /// Loaded plugin instances, in config order, with hook and service indexes.
    pub plugins: Arc<PluginTable>,
    /// One state slot per plugin, parallel to `plugins.entries()`.
    pub state: Vec<Box<dyn PluginState>>,
    /// The session log and the view folded from it.
    pub journal: Journal,
    /// This agent's consumer on its queue. `None` before the runtime attaches it, after a
    /// quiesce detaches it, and briefly while [`AgentCtx::pull_messages`] holds it.
    pub queue: Option<Box<dyn QueueConsumer>>,
    /// What the agent is doing.
    pub phase: Phase,
    /// How the agent was loaded in this process.
    pub loaded_by: LoadKind,
    /// Cancelled to cancel the current turn. Replaced at the start of each turn.
    pub turn_cancel: CancellationToken,
    /// Answers received for questions plugins asked during the current turn. Cleared when
    /// a new turn starts, so answers to questions nobody is waiting for don't accumulate.
    pub answers: HashMap<RequestId, Answer>,
    /// Where live output goes.
    pub observer: Observer,
    /// Set while the runtime replays the log during recovery; hooks can't append then.
    pub(crate) replaying: bool,
    /// The pending drain, if one was requested and not settled yet.
    drain: Option<DrainState>,
    /// Set when a drain command arrived and `drain_requested` hasn't been dispatched since.
    drain_hook_pending: bool,
    /// Receives commands from the agent's handles.
    commands: mpsc::Receiver<Command>,
    /// Set once every handle is dropped, so waits stop polling the closed channel.
    commands_closed: bool,
}

impl AgentCtx {
    /// Builds a context around loaded plugins and an open journal, in phase `Idle` and with
    /// no queue attached. Returns the sender for the command channel, from which the runtime
    /// makes the agent's handle.
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
            drain_hook_pending: false,
            commands,
            commands_closed: false,
        };
        (cx, tx)
    }

    /// Starts a fresh cancellation scope for a new turn and drops answers left over from
    /// the previous one. Call it before logging `TurnStarted`, so a cancel aimed at the
    /// previous turn doesn't leak into this one.
    pub fn begin_turn_scope(&mut self) {
        self.turn_cancel = CancellationToken::new();
        self.answers.clear();
    }

    /// The point a pending drain asked for, if one is pending.
    pub fn draining(&self) -> Option<DrainPoint> {
        self.drain.as_ref().map(|d| d.at)
    }

    /// The tool context for one call, with the providing plugin's grants. Its cancel token is
    /// a child of the turn's, so it fires whenever the turn is cancelled.
    pub fn tool_cx(&self, entry: &PluginEntry, call_id: CallId) -> ToolCx {
        ToolCx {
            agent: self.id.clone(),
            tenant: self.tenant.clone(),
            call_id,
            grants: entry.grants.clone(),
            cancel: self.turn_cancel.child_token(),
        }
    }

    /// Applies one command to the context. Never blocks: a drain is only recorded here and
    /// settled later by [`AgentCtx::settle_drain`].
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
                self.drain_hook_pending = true;
            }
        }
    }

    /// Turns the pending drain into an immediate one after its deadline passed: marks it
    /// forced and cancels the turn, so the agent quiesces at the next turn boundary.
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

    /// Handles every command already waiting, without blocking. Doesn't dispatch
    /// `drain_requested`; [`AgentCtx::apply_pending_commands`] does.
    pub fn poll_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            self.handle_command(command);
        }
    }

    /// Polls `fut` until it completes, a command arrives, or a drain deadline passes.
    /// Returns `None` after handling a command or a deadline; the future can be polled again.
    ///
    /// The future is checked first, so a result that's ready wins over a command. The
    /// deadline counts only for a drain that hasn't been forced yet. This is the primitive
    /// for waits that need to react to each command, such as waiting for an approval
    /// answer; [`AgentCtx::wait`] covers the common case.
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
    ///
    /// Commands are handled as they arrive, so a cancel or a forced drain cancels the turn
    /// token while `fut` is still running. The driver uses it around model requests, tool
    /// bodies and retry delays.
    pub async fn wait<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        loop {
            if let Some(out) = self.wait_one(fut.as_mut()).await {
                return out;
            }
        }
    }

    /// Handles waiting commands and dispatches `drain_requested` once for any drain
    /// commands that arrived since the last call, with the drain point now in effect (the
    /// strictest one asked for). The driver calls it at each safe point, before
    /// [`AgentCtx::settle_drain`]. Fails if a `drain_requested` listener fails.
    pub async fn apply_pending_commands(&mut self) -> Result<()> {
        self.poll_commands();
        if std::mem::take(&mut self.drain_hook_pending)
            && let Some(at) = self.draining()
        {
            dispatch(self, &mut HookEvent::DrainRequested { at }).await?;
        }
        Ok(())
    }

    /// Pulls up to `max` messages, logs the new ones, syncs, then acknowledges them. With
    /// `wait`, waits for a message, but returns early (with 0) when a command arrives.
    /// Does nothing while a drain is pending, so new messages stay in the queue.
    ///
    /// Returns how many messages were logged. A message whose id is no newer than the last
    /// one logged is a redelivery (the agent stopped between sync and ack) and is skipped,
    /// though it's still acknowledged. Because the sync comes before the ack, a message
    /// leaves the queue only once it's durable in the log. Fails if the queue isn't attached
    /// or the queue or log fails; the consumer is put back either way.
    pub async fn pull_messages(&mut self, max: usize, wait: bool) -> Result<usize> {
        if self.drain.is_some() {
            return Ok(0);
        }
        let mut queue = self
            .queue
            .take()
            .ok_or_else(|| Error::Queue("the agent is not attached to its queue".into()))?;
        // The consumer is taken out of `self` so the pull future can run while `wait_one`
        // borrows the context; it goes back before any error is returned.
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
    ///
    /// The driver calls it at each safe point with the boundary it's at: `Step` between
    /// steps of a turn, `Turn` between turns. Returns `None` when there's no drain or `here`
    /// doesn't satisfy it, and `Some(Exit::Quiesced)` after quiescing, in which case the
    /// driver must return without touching the log again. At a step boundary the open turn
    /// is recorded as suspended, so thaw continues it.
    ///
    /// If any step fails, every waiter receives [`DrainResult::Failed`] with the error, and
    /// the error is returned so the driver stops; the agent can then be recovered from its log.
    pub async fn settle_drain(&mut self, here: DrainPoint) -> Result<Option<Exit>> {
        match &self.drain {
            Some(d) if here.satisfies(d.at) => {}
            _ => return Ok(None),
        }
        // Snapshot first, while the drain is still pending, so plugins write their state into
        // their slots before the image reads them.
        dispatch(self, &mut HookEvent::Snapshot).await?;
        let Some(drain) = self.drain.take() else {
            return Ok(None);
        };
        let forced = drain.forced;
        match self.quiesce(here, forced).await {
            Ok(image) => {
                self.observer.emit(Observation::Quiesced { forced });
                for waiter in drain.waiters {
                    let _ = waiter.send(DrainResult::Quiesced {
                        image: Box::new(image.clone()),
                        forced,
                    });
                }
                Ok(Some(Exit::Quiesced))
            }
            Err(e) => {
                for waiter in drain.waiters {
                    let _ = waiter.send(DrainResult::Failed(e.to_string()));
                }
                Err(e)
            }
        }
    }

    /// The fallible part of [`AgentCtx::settle_drain`]: logs `Quiesced`, syncs, builds the
    /// image, releases the log and detaches from the queue. At a step boundary the open
    /// turn is recorded as suspended.
    async fn quiesce(&mut self, here: DrainPoint, forced: bool) -> Result<AgentImage> {
        let suspended_turn = if here == DrainPoint::Step {
            self.journal.view().open_turn
        } else {
            None
        };
        self.journal.append(Event::Quiesced {
            at: here,
            suspended_turn,
            forced,
        })?;
        self.journal.sync().await?;
        let image = self.to_image()?;
        self.journal.release().await?;
        if let Some(queue) = self.queue.take() {
            queue.detach().await?;
        }
        self.phase = Phase::Quiesced;
        Ok(image)
    }

    /// Tells every drain waiter the agent stopped without an image. Called by the runtime
    /// when the driver returns an error. Polls the command channel first, so drains that
    /// were sent but not yet received get the answer too.
    pub fn fail_drain_waiters(&mut self, reason: &str) {
        self.poll_commands();
        if let Some(drain) = self.drain.take() {
            for waiter in drain.waiters {
                let _ = waiter.send(DrainResult::Failed(reason.to_string()));
            }
        }
    }

    /// The whole context as data: config, every slot, the view and the log position.
    ///
    /// The position includes unsynced events, so take the image right after a sync, as
    /// [`AgentCtx::settle_drain`] does, or it may describe events the log never kept. Fails
    /// if a slot can't be serialized.
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
///
/// Every method except [`AgentHandle::send`] and [`AgentHandle::subscribe`] goes through
/// the command channel and fails with [`Error::AgentGone`] once the agent's task has ended.
/// `send` talks to the queue plugin directly, so it keeps working after the agent quiesces;
/// the message waits for whoever thaws the agent.
#[derive(Clone)]
pub struct AgentHandle {
    /// The agent's id.
    pub id: AgentId,
    /// The agent's tenant.
    pub tenant: TenantId,
    /// Sender side of the agent's command channel.
    commands: mpsc::Sender<Command>,
    /// The agent's observer, for subscribing.
    observer: Observer,
    /// The agent's message queue plugin, for publishing.
    queue: Arc<dyn Plugin>,
}

impl AgentHandle {
    /// Makes the handle for a context, from the sender returned by [`AgentCtx::new`].
    pub(crate) fn new(cx: &AgentCtx, commands: mpsc::Sender<Command>) -> Self {
        Self {
            id: cx.id.clone(),
            tenant: cx.tenant.clone(),
            commands,
            observer: cx.observer.clone(),
            queue: cx.plugins.queue().clone(),
        }
    }

    /// Publishes a message to the agent's queue and returns its queue id. The agent sees it
    /// the next time it pulls: at once if idle, at the next step boundary if `delivery` is
    /// [`Delivery::NextStep`] and a turn is running, or after the current turn otherwise.
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

    /// Cancels the current turn. Returns once the command is queued, not when the turn
    /// has ended.
    pub async fn cancel(&self) -> Result<()> {
        self.commands
            .send(Command::Cancel)
            .await
            .map_err(|_| Error::AgentGone)
    }

    /// Answers a question a plugin asked, such as an
    /// [`Observation::ApprovalRequested`]. An answer to a question nobody is waiting for is
    /// kept and ignored.
    pub async fn answer(&self, request: RequestId, answer: Answer) -> Result<()> {
        self.commands
            .send(Command::Answer { request, answer })
            .await
            .map_err(|_| Error::AgentGone)
    }

    /// Drains the agent at `at`, forcing an immediate stop after `timeout`, and returns its image.
    ///
    /// Waits until the agent quiesces or fails. Fails with [`Error::AgentGone`] if the task
    /// ended without replying, such as after a panic or an earlier drain that already
    /// quiesced it.
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
    /// How to reach the agent.
    pub handle: AgentHandle,
    /// The agent's task; resolves to the driver's result once it quiesces or fails.
    pub task: JoinHandle<Result<Exit>>,
}
