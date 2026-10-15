//! Hooks: typed interception points the driver dispatches to subscribed plugins, in order.

use crate::agent::{AgentCtx, DrainPoint, LoadKind};
use crate::error::{Error, Result};
use crate::event::{Delivery, Event, Source, TurnEnd, View};
use crate::ids::{AgentId, PluginId, TenantId, TurnId};
use crate::message::{AssistantMessage, ModelError, Route, ToolCall, ToolResult};
use crate::plugin::PluginState;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Names of the hooks, used in subscriptions and in `grants.hooks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookName {
    AgentLoaded,
    PreStep,
    Request,
    RequestError,
    Response,
    ToolPre,
    ToolPost,
    TurnStopping,
    TurnEnded,
    Snapshot,
    Replay,
    DrainRequested,
}

/// A plugin's interest in one hook. Listeners run in ascending `order`; ties keep the
/// plugins' order in the agent config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub hook: HookName,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorDecision {
    Fail,
    /// Retry after waiting `after`. The driver waits, so a cancel or drain still gets through.
    Retry {
        after: std::time::Duration,
    },
}

/// The `response` decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseVerdict {
    Proceed,
    StopTurn,
}

/// The `tool_pre` decision for one call.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolDecision {
    Allow,
    /// Do not run; the reason becomes the call's model-visible result.
    Deny {
        reason: String,
    },
    /// Ask whoever holds the agent's handle, and wait for an answer.
    Ask {
        question: String,
    },
}

/// One hook dispatch and its payload. Listeners may change what the `&mut` fields point to.
pub enum HookEvent<'a> {
    /// After create, thaw or recovery, before any work.
    AgentLoaded { how: LoadKind },
    /// At the start of each step. Listeners add context with [`HookCx::queue_context`].
    PreStep,
    /// Before each model attempt.
    Request { route: &'a mut Route },
    /// After a failed attempt.
    RequestError {
        error: &'a ModelError,
        decision: &'a mut ErrorDecision,
    },
    /// After the reply is logged, before tools run.
    Response {
        reply: &'a AssistantMessage,
        verdict: &'a mut ResponseVerdict,
    },
    /// Per tool call, in model order, before anything runs.
    ToolPre {
        call: &'a mut ToolCall,
        decision: &'a mut ToolDecision,
    },
    /// Per result, in model order, before it is committed.
    ToolPost { result: &'a mut ToolResult },
    /// When the turn would end: listeners may queue next-step context to force another step.
    TurnStopping,
    /// After the turn's end is logged.
    TurnEnded { turn: TurnId, end: &'a TurnEnd },
    /// Before an image is taken: plugins with state outside their slot write it into the slot.
    Snapshot,
    /// For each event replayed during crash recovery. Listeners rebuild their slot and must
    /// have no other effect; [`HookCx::append`] fails while replaying.
    Replay { event: &'a Event },
    /// When a drain is requested: stop admitting background work.
    DrainRequested { at: DrainPoint },
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
pub struct HookCx<'a> {
    pub plugin: &'a PluginId,
    pub agent: &'a AgentId,
    pub tenant: &'a TenantId,
    journal: &'a mut crate::log::Journal,
    state: &'a mut dyn PluginState,
    replaying: bool,
    cancel: &'a CancellationToken,
}

impl HookCx<'_> {
    /// Everything derived from the log: history, inbox, counters, open work.
    pub fn view(&self) -> &View {
        self.journal.view()
    }

    /// Logs an event; this is how a listener changes anything the model will see.
    pub fn append(&mut self, event: Event) -> Result<()> {
        if self.replaying {
            return Err(Error::plugin(
                self.plugin,
                "cannot append to the log while replaying",
            ));
        }
        self.journal.append(event)
    }

    /// Queues context from this plugin for the next step or the next turn.
    pub fn queue_context(&mut self, delivery: Delivery, text: impl Into<String>) -> Result<()> {
        let source = Source::Plugin(self.plugin.clone());
        self.append(Event::ContextQueued {
            source,
            delivery,
            text: text.into(),
        })
    }

    /// This plugin's state slot, as the type its runner created it with.
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
pub async fn dispatch(cx: &mut AgentCtx, event: &mut HookEvent<'_>) -> Result<Flow> {
    let hook = event.name();
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
