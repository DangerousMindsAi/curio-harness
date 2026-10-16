//! Hooks: typed interception points the driver dispatches to subscribed plugins, in order.
//!
//! A hook is how a plugin that isn't a service takes part in the loop. The driver (or the
//! runtime, for `replay`) builds a [`HookEvent`] with the hook's payload and calls
//! [`dispatch`], which runs every plugin subscribed to that [`HookName`] in turn. A plugin
//! declares its interest with [`Subscription`]s from [`crate::Plugin::subscriptions`];
//! [`crate::PluginTable::build`] sorts subscribers by `order` and then by config position
//! once, at load, and checks them against each plugin's `grants.hooks`.
//!
//! Each listener gets a [`HookCx`] and the event. Payload fields behind `&mut` are the
//! listener's to change (a route, a decision, a tool call or result), and each later
//! listener sees the change. The listener's return value, a [`Flow`], says whether the
//! chain continues. Continuing is the default; stopping or rejecting is explicit.
//!
//! Two rules keep hooks safe for checkpointing. A listener that wants to change what the
//! model sees appends an event through [`HookCx::append`] (or [`HookCx::queue_context`]),
//! so the change is in the log and survives recovery. And a listener's own mutable state
//! lives in its state slot, reached through [`HookCx::state`], so it's captured in every
//! image. During crash recovery the `replay` hook lets plugins rebuild that state from the
//! log, and appending is refused so replay can't cause side effects.

use crate::agent::{AgentCtx, DrainPoint, LoadKind};
use crate::error::{Error, Result};
use crate::event::{Delivery, Event, Source, TurnEnd, View};
use crate::ids::{AgentId, PluginId, TenantId, TurnId};
use crate::message::{AssistantMessage, ModelError, Route, ToolCall, ToolResult};
use crate::plugin::PluginState;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Names of the hooks, used in subscriptions and in `grants.hooks`. Each one matches a
/// [`HookEvent`] variant, which documents when it's dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookName {
    /// See [`HookEvent::AgentLoaded`].
    AgentLoaded,
    /// See [`HookEvent::PreStep`].
    PreStep,
    /// See [`HookEvent::Request`].
    Request,
    /// See [`HookEvent::RequestError`].
    RequestError,
    /// See [`HookEvent::Response`].
    Response,
    /// See [`HookEvent::ToolPre`].
    ToolPre,
    /// See [`HookEvent::ToolPost`].
    ToolPost,
    /// See [`HookEvent::TurnStopping`].
    TurnStopping,
    /// See [`HookEvent::TurnEnded`].
    TurnEnded,
    /// See [`HookEvent::Snapshot`].
    Snapshot,
    /// See [`HookEvent::Replay`].
    Replay,
    /// See [`HookEvent::DrainRequested`].
    DrainRequested,
}

/// A plugin's interest in one hook. Listeners run in ascending `order`; ties keep the
/// plugins' order in the agent config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subscription {
    /// The hook to listen to.
    pub hook: HookName,
    /// Sort key among the hook's listeners; lower runs first.
    pub order: i32,
}

impl Subscription {
    /// A subscription with the default order, 0.
    pub fn new(hook: HookName) -> Self {
        Self { hook, order: 0 }
    }
}

/// What a listener returns. Continuing is the default; stopping is explicit.
#[derive(Clone, Debug, PartialEq)]
pub enum Flow {
    /// Pass the payload, possibly changed, to the next listener.
    Continue,
    /// Later listeners do not run.
    Stop,
    /// Refuse the operation. Only `pre_step` accepts it; the turn ends blocked.
    Reject(String),
}

/// The `request_error` decision. A listener sets `Retry` only when a retry can succeed.
///
/// The driver starts each dispatch at `Fail`, so with no retry policy a failed attempt
/// ends the step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorDecision {
    /// End the step as failed, and the turn with [`TurnEnd::Error`].
    Fail,
    /// Retry after waiting `after`. The driver waits, so a cancel or drain still gets through.
    Retry {
        /// How long to wait before the next attempt.
        after: std::time::Duration,
    },
}

/// The `response` decision. Starts at `Proceed` for each reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseVerdict {
    /// Run the reply's tool calls, if any, as usual.
    Proceed,
    /// End the turn after this reply. Its tool calls don't run; each gets a synthetic
    /// "not run" result so the conversation stays well formed.
    StopTurn,
}

/// The `tool_pre` decision for one call. Starts at `Allow` for each call.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolDecision {
    /// Run the call.
    Allow,
    /// Do not run; the reason becomes the call's model-visible result.
    Deny {
        /// Why, as the model will see it.
        reason: String,
    },
    /// Ask whoever holds the agent's handle, and wait for an answer. The driver logs the
    /// question, emits it as an observation, and turns the answer (or a cancelled turn)
    /// into `Allow` or `Deny`.
    Ask {
        /// What to ask, shown to whoever answers.
        question: String,
    },
}

/// One hook dispatch and its payload. Listeners may change what the `&mut` fields point to.
///
/// The variants are listed in loop order. The plain borrows (`&'a`) are read-only context;
/// the `&'a mut` fields are outputs the dispatcher's caller reads after every listener ran.
pub enum HookEvent<'a> {
    /// After create, thaw or recovery, before any work. Dispatched by the driver at the start
    /// of `run`, every time the agent is loaded, so listeners that should act once (such as
    /// writing a system prompt) keep a flag in their slot.
    AgentLoaded {
        /// How the agent came to run in this process.
        how: LoadKind,
    },
    /// At the start of each step, after steering messages were pulled and before the step's
    /// input is admitted. Listeners add context with [`HookCx::queue_context`], and may
    /// return [`Flow::Reject`] to end the turn as blocked.
    PreStep,
    /// Before each model attempt, including retries.
    Request {
        /// The route for this attempt, starting from the driver's configured defaults.
        route: &'a mut Route,
    },
    /// After a failed attempt, once `AttemptFailed` is logged. Not dispatched for a
    /// cancelled attempt.
    RequestError {
        /// What went wrong.
        error: &'a ModelError,
        /// Whether to retry; starts at [`ErrorDecision::Fail`].
        decision: &'a mut ErrorDecision,
    },
    /// After the reply is logged, before tools run.
    Response {
        /// The logged reply.
        reply: &'a AssistantMessage,
        /// Whether the turn goes on; starts at [`ResponseVerdict::Proceed`].
        verdict: &'a mut ResponseVerdict,
    },
    /// Per tool call, in model order, before anything runs.
    ToolPre {
        /// The call, which a listener may rewrite (for example, its arguments).
        call: &'a mut ToolCall,
        /// What to do with it; starts at [`ToolDecision::Allow`].
        decision: &'a mut ToolDecision,
    },
    /// Per result, in model order, before it is committed.
    ToolPost {
        /// The result about to be logged; a listener may rewrite it or set `concludes_turn`.
        result: &'a mut ToolResult,
    },
    /// When the turn would end: listeners may queue next-step context to force another step.
    /// Dispatched only when nothing is already waiting for the next step.
    TurnStopping,
    /// After the turn's end is logged and synced.
    TurnEnded {
        /// The turn that ended.
        turn: TurnId,
        /// How it ended.
        end: &'a TurnEnd,
    },
    /// Before an image is taken: plugins with state outside their slot write it into the slot.
    Snapshot,
    /// For each event replayed during crash recovery. Listeners rebuild their slot and must
    /// have no other effect; [`HookCx::append`] fails while replaying.
    Replay {
        /// The event just folded into the view.
        event: &'a Event,
    },
    /// When a drain is requested: stop admitting background work. Dispatched the next time
    /// the driver applies pending commands after a drain command arrives; several drain
    /// commands that arrive in between give one dispatch.
    DrainRequested {
        /// The drain point the newest drain command asked for.
        at: DrainPoint,
    },
}

impl HookEvent<'_> {
    /// The hook this event dispatches to.
    pub fn name(&self) -> HookName {
        match self {
            HookEvent::AgentLoaded { .. } => HookName::AgentLoaded,
            HookEvent::PreStep => HookName::PreStep,
            HookEvent::Request { .. } => HookName::Request,
            HookEvent::RequestError { .. } => HookName::RequestError,
            HookEvent::Response { .. } => HookName::Response,
            HookEvent::ToolPre { .. } => HookName::ToolPre,
            HookEvent::ToolPost { .. } => HookName::ToolPost,
            HookEvent::TurnStopping => HookName::TurnStopping,
            HookEvent::TurnEnded { .. } => HookName::TurnEnded,
            HookEvent::Snapshot => HookName::Snapshot,
            HookEvent::Replay { .. } => HookName::Replay,
            HookEvent::DrainRequested { .. } => HookName::DrainRequested,
        }
    }
}

/// What a listener sees: its own state slot mutably, and the rest of the agent only
/// through the view and the log.
///
/// Built fresh by [`dispatch`] for each listener and borrowed from the [`AgentCtx`] for the
/// length of one `on_hook` call. It can't reach other plugins' slots, the queue, the
/// command channel or the log's sync, which keeps those under the driver's control.
pub struct HookCx<'a> {
    /// The listening plugin's id.
    pub plugin: &'a PluginId,
    /// The agent being driven.
    pub agent: &'a AgentId,
    /// The agent's tenant.
    pub tenant: &'a TenantId,
    /// The agent's journal, for reading the view and appending.
    journal: &'a mut crate::log::Journal,
    /// The listening plugin's own state slot.
    state: &'a mut dyn PluginState,
    /// Set during crash recovery, when appending is refused.
    replaying: bool,
    /// The current turn's cancellation token.
    cancel: &'a CancellationToken,
}

impl HookCx<'_> {
    /// Everything derived from the log: history, inbox, counters, open work.
    pub fn view(&self) -> &View {
        self.journal.view()
    }

    /// Logs an event; this is how a listener changes anything the model will see. The event
    /// is folded into the view at once and becomes durable at the driver's next save point.
    /// Fails while replaying.
    pub fn append(&mut self, event: Event) -> Result<()> {
        if self.replaying {
            return Err(Error::plugin(
                self.plugin,
                "cannot append to the log while replaying",
            ));
        }
        self.journal.append(event)
    }

    /// Queues context from this plugin for the next step or the next turn, as a
    /// [`Event::ContextQueued`] with [`Source::Plugin`]. The model sees it once a step or
    /// turn admits it.
    pub fn queue_context(&mut self, delivery: Delivery, text: impl Into<String>) -> Result<()> {
        let source = Source::Plugin(self.plugin.clone());
        self.append(Event::ContextQueued {
            source,
            delivery,
            text: text.into(),
        })
    }

    /// This plugin's state slot, as the type its runner created it with. Fails if `T` isn't
    /// that type.
    pub fn state<T: 'static>(&mut self) -> Result<&mut T> {
        let plugin = self.plugin;
        self.state
            .as_any_mut()
            .downcast_mut::<T>()
            .ok_or_else(|| Error::plugin(plugin, "state slot has an unexpected type"))
    }

    /// Whether this dispatch is part of crash recovery.
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// Cancelled when the current turn is cancelled. Listeners that wait should watch it.
    pub fn cancel(&self) -> &CancellationToken {
        self.cancel
    }
}

/// Dispatches one hook to every subscribed plugin in order. Returns the first `Stop` or
/// `Reject`, or `Continue`. A listener's error fails the dispatch.
///
/// Listeners run one at a time, each awaited before the next starts, so they never race on
/// the payload or the log. A `Reject` from any hook other than `pre_step` is treated as a
/// plugin error. Events appended by listeners before a failure stay in the journal.
///
/// Called by the driver for loop hooks, by [`AgentCtx`] for `drain_requested` and
/// `snapshot`, and by [`crate::Runtime::recover`] for `replay`.
pub async fn dispatch(cx: &mut AgentCtx, event: &mut HookEvent<'_>) -> Result<Flow> {
    let hook = event.name();
    // A clone of the `Arc`, so the table stays borrowed while `cx` is borrowed mutably below.
    let plugins = cx.plugins.clone();
    let replaying = cx.replaying;
    for &index in plugins.subscribers(hook) {
        let entry = &plugins.entries()[index];
        let mut hcx = HookCx {
            plugin: &entry.id,
            agent: &cx.id,
            tenant: &cx.tenant,
            journal: &mut cx.journal,
            state: cx.state[index].as_mut(),
            replaying,
            cancel: &cx.turn_cancel,
        };
        let flow = entry.plugin.on_hook(&mut hcx, event).await?;
        match flow {
            Flow::Continue => {}
            Flow::Stop => return Ok(Flow::Stop),
            Flow::Reject(reason) if hook == HookName::PreStep => return Ok(Flow::Reject(reason)),
            Flow::Reject(_) => {
                return Err(Error::plugin(
                    &entry.id,
                    format!("{hook:?} listeners cannot reject"),
                ));
            }
        }
    }
    Ok(Flow::Continue)
}
