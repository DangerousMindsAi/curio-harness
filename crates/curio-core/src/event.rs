//! Session log events and the view folded from them.
//!
//! An [`Event`] is one entry of an agent's session log. The driver and hook listeners
//! append events for everything that happens to the agent: messages pulled from its queue,
//! context plugins queue, the start and end of every turn and step, model replies and
//! failed attempts, tool calls being started, denied, approved and answered, and the
//! lifecycle markers `Quiesced`, `Thawed` and `Recovered`.
//!
//! The [`View`] is everything about an agent that can be derived from its log: the
//! model-visible [`History`], the [`Inbox`] of input waiting to be used, the [`Counters`]
//! limits are checked against, and open work (the open turn and step, and tool calls with
//! no result yet). It changes only by folding events with [`View::fold`], both live
//! (through [`crate::Journal::append`]) and during crash recovery (through
//! [`crate::Journal::replay`]), so a recovered agent sees exactly what the live one saw.
//! That's also why `fold` must stay deterministic and depend on nothing but the view and
//! the event.
//!
//! Input reaches the model in two stages. First it's logged into the inbox, either as a
//! [`QueuedMessage`] pulled from the queue or as context a plugin or the runtime queued;
//! each item gets an [`InboxItemId`] and a [`Delivery`] that says whether it waits for the
//! next turn or the next step. Then a `TurnStarted` or `StepStarted` event admits a list
//! of those ids, which moves the items out of the inbox and into the history. Naming the
//! admitted ids in the event, instead of "everything waiting", keeps replay exact even if
//! more input was logged in between.

use crate::agent::DrainPoint;
use crate::ids::{CallId, InboxItemId, PluginId, QueueMsgId, RequestId, TurnId};
use crate::message::{AssistantMessage, ChatMessage, Role, ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

/// When a queued message or context item should be used. Decides which inbox list the
/// item waits in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Starts, or joins the start of, the next turn.
    #[default]
    NextTurn,
    /// Joins the next step of the running turn (steering), or the first step of the next turn.
    NextStep,
}

/// A message as the queue delivered it. Logged whole in [`Event::MessageReceived`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessage {
    /// The queue's id for the message; increases within one agent's queue.
    pub id: QueueMsgId,
    /// When the sender sent it. Logged for diagnosis; never used for ordering.
    pub sent_at_ms: u64,
    /// Whether the message starts a turn or steers the running one.
    pub delivery: Delivery,
    /// The message text, shown to the model as is.
    pub body: String,
}

/// Where an inbox item came from. Decides how the item is labelled for the model; see
/// [`InboxItem::to_message`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Source {
    /// A message pulled from the agent's queue.
    Queue(QueueMsgId),
    /// Context a plugin queued through [`crate::HookCx::queue_context`].
    Plugin(PluginId),
    /// A notice from the runtime itself, such as [`crate::RESTARTED_NOTICE`].
    System,
}

/// Input that is logged and waiting to be used by a turn or step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    /// Assigned when the item is folded; what `TurnStarted` and `StepStarted` admit by.
    pub id: InboxItemId,
    /// Who the item came from.
    pub source: Source,
    /// The item's text, before any label is added.
    pub text: String,
}

impl InboxItem {
    /// How this item appears in the model-visible history: a user message, with plugin
    /// context and system notices labelled so the model can tell them from the user.
    pub fn to_message(&self) -> ChatMessage {
        let text = match &self.source {
            Source::Queue(_) => self.text.clone(),
            Source::Plugin(p) => format!("[context from {p}]\n{}", self.text),
            Source::System => format!("[system notice]\n{}", self.text),
        };
        ChatMessage::text(Role::User, text)
    }
}

/// How a step ended. Logged in [`Event::StepEnded`]; the driver maps each outcome except
/// `ToolsRan` to a [`TurnEnd`] or to the turn-stopping check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepOutcome {
    /// Tools ran, so the model is owed another request.
    ToolsRan,
    /// The model answered without tool calls.
    Answered,
    /// A plugin ended the turn: a `response` listener returned `StopTurn`, or a `tool_post`
    /// listener set `concludes_turn`.
    Concluded,
    /// The reply hit the output token limit.
    MaxTokens,
    /// A model attempt failed and no `request_error` listener chose to retry.
    Failed {
        /// The last attempt's error, as text.
        error: String,
    },
    /// The turn was cancelled during the step.
    Cancelled,
    /// The process stopped during the step; written by crash repair.
    Interrupted,
}

/// How a turn ended. Logged in [`Event::TurnEnded`], except for `Suspended`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEnd {
    /// The model answered (or a plugin concluded the turn) and nothing asked for another
    /// step.
    Completed,
    /// The turn reached the driver's per-turn step limit.
    StepLimit,
    /// Plugins kept forcing more steps after the model had answered, past the driver's
    /// limit on forced continuations.
    ContinuationLimit,
    /// The last reply hit the output token limit.
    MaxTokens,
    /// A `pre_step` listener rejected the step.
    Blocked {
        /// The listener's reason.
        reason: String,
    },
    /// The turn was cancelled: by [`crate::Command::Cancel`], by an immediate drain, or
    /// because a drain's deadline passed.
    Cancelled,
    /// A model attempt failed and wasn't retried.
    Error {
        /// The attempt's error, as text.
        message: String,
    },
    /// The process stopped during the turn; written by crash repair.
    Interrupted,
    /// Drained at a step boundary. Never logged as a `TurnEnded`: the turn stays open and
    /// continues when the agent is thawed.
    Suspended,
}

/// One entry of an agent's session log.
///
/// Events are facts about the past and are never edited. The `type` tag and field names are
/// the log's encoding, versioned by [`crate::log::LOG_FORMAT`]. Variants that change what
/// the model sees, or what recovery needs to know, are folded by [`View::fold`]; the rest
/// (denials, approvals and lifecycle markers) are kept for audit and for plugins that watch
/// them in the `replay` hook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sets the system prompt contributed by one plugin, replacing its earlier one.
    SystemPrompt {
        /// The contributing plugin.
        source: PluginId,
        /// Its system prompt text.
        text: String,
    },
    /// A message pulled from the agent's queue. Logged before the queue is acknowledged.
    /// Messages whose id is no newer than [`Inbox::last_logged`] are never logged again.
    MessageReceived {
        /// The message as the queue delivered it.
        message: QueuedMessage,
    },
    /// Context added by a plugin or by the runtime, waiting for a turn or step.
    ContextQueued {
        /// Who queued it; never [`Source::Queue`] in practice.
        source: Source,
        /// Which inbox list it waits in.
        delivery: Delivery,
        /// The context text.
        text: String,
    },
    /// A turn started with these next-turn items as its input.
    TurnStarted {
        /// The new turn's id, from [`View::next_turn_id`].
        turn: TurnId,
        /// The `next_turn` items moved into the history.
        admitted: Vec<InboxItemId>,
    },
    /// A step started with these next-step items added to the history. Never repeated on retry.
    StepStarted {
        /// The `next_step` items moved into the history.
        admitted: Vec<InboxItemId>,
    },
    /// One model attempt failed. Counted in [`Counters::failed_attempts_this_step`]; the
    /// step may still retry.
    AttemptFailed {
        /// The model error, as text.
        error: String,
    },
    /// A complete model reply. Its tool calls become pending until each has a result.
    AssistantReply {
        /// The reply as the provider returned it (with tool calls dropped when it was cut
        /// off by the token limit).
        reply: AssistantMessage,
    },
    /// A `tool_pre` listener rewrote a call's name or arguments. Logged when the listeners
    /// are done, before the call is denied, asked about or run, so the log records what
    /// actually ran. The history keeps the call as the model made it; only the pending call
    /// changes.
    ToolCallRewritten {
        /// The call, whose id listeners may not change.
        call_id: CallId,
        /// The tool name after the rewrite.
        name: String,
        /// The arguments after the rewrite, as JSON text.
        arguments: String,
    },
    /// A `tool_pre` listener denied the call; the reason is the call's model-visible result.
    /// The `ToolResult` that carries the reason follows later, when results are committed.
    ToolDenied {
        /// The denied call.
        call_id: CallId,
        /// Why, as the model will see it.
        reason: String,
    },
    /// A `tool_pre` listener asked for a human decision on a call. Logged and synced before
    /// the question goes out.
    ApprovalRequested {
        /// The id the answer must come back under.
        request: RequestId,
        /// The call in question.
        call_id: CallId,
        /// What the listener asked.
        question: String,
    },
    /// The answer to an `ApprovalRequested`, as delivered by [`crate::Command::Answer`].
    ApprovalAnswered {
        /// The request this answers.
        request: RequestId,
        /// Whether the call may run.
        approved: bool,
        /// The answerer's note, passed on to the model when the call is denied.
        note: Option<String>,
    },
    /// Written and synced before the tool body runs, so recovery can tell "may have run"
    /// from "never ran".
    ToolStarted {
        /// The call whose body is about to run.
        call_id: CallId,
    },
    /// The committed result of one call, after `tool_post` listeners ran. Also written for
    /// calls that never ran, as a synthetic result.
    ToolResult {
        /// The result as the model will see it.
        result: ToolResult,
    },
    /// The open step ended.
    StepEnded {
        /// How it ended.
        outcome: StepOutcome,
    },
    /// The open turn ended. Never written for a suspended turn.
    TurnEnded {
        /// The turn that ended.
        turn: TurnId,
        /// How it ended; never [`TurnEnd::Suspended`].
        end: TurnEnd,
    },
    /// The agent quiesced at a safe point and its image was taken right after this event.
    Quiesced {
        /// The safe point it stopped at: `Step` or `Turn`.
        at: DrainPoint,
        /// The turn left open by a drain at a step boundary, if any.
        suspended_turn: Option<TurnId>,
        /// Whether the drain deadline passed and the turn was cut short.
        forced: bool,
    },
    /// An image was thawed and this host took over the log.
    Thawed {
        /// The thawing host's id.
        host: String,
        /// The epoch this host opened the log with.
        epoch: u64,
    },
    /// The agent was rebuilt from its log after it stopped without quiescing.
    Recovered {
        /// The recovering host's id.
        host: String,
        /// The epoch this host opened the log with.
        epoch: u64,
        /// How many dangling tool calls crash repair answered.
        repaired_calls: usize,
    },
}

/// The model-visible conversation. Built only by [`View::fold`], so it can always be
/// rebuilt from the log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    /// System prompt parts, one per contributing plugin, in first-contribution order.
    pub system: Vec<(PluginId, String)>,
    /// Every admitted input, reply and tool result, in log order.
    pub messages: Vec<ChatMessage>,
}

impl History {
    /// The messages a model request is built from: one system message joining every part
    /// with a blank line (omitted when there are none), then the conversation.
    pub fn to_request_messages(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if !self.system.is_empty() {
            let text = self
                .system
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            out.push(ChatMessage::text(Role::System, text));
        }
        out.extend(self.messages.iter().cloned());
        out
    }
}

/// Logged input not yet used by a turn or step.
///
/// Both lists keep log order. The driver starts a turn only when `next_turn` has items, and
/// starts another step after the model answers only when `next_step` has items, so
/// continuing a turn is always a matter of logged data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    /// Items waiting to start the next turn. The default driver admits all of them in the
    /// turn's `TurnStarted`.
    pub next_turn: Vec<InboxItem>,
    /// Items waiting for the next step, of this turn or the next; admitted by `StepStarted`.
    pub next_step: Vec<InboxItem>,
    /// The newest queue message in the log; redelivered messages up to it are skipped.
    pub last_logged: Option<QueueMsgId>,
    /// The last [`InboxItemId`] handed out; ids are never reused.
    next_item: u64,
}

impl Inbox {
    /// Gives a new item the next id and adds it to the list `delivery` names.
    fn push(&mut self, source: Source, delivery: Delivery, text: String) {
        self.next_item += 1;
        let item = InboxItem {
            id: InboxItemId(self.next_item),
            source,
            text,
        };
        match delivery {
            Delivery::NextTurn => self.next_turn.push(item),
            Delivery::NextStep => self.next_step.push(item),
        }
    }

    /// Removes and returns the items of `list` whose ids are in `ids`, keeping the order of
    /// both the taken and the remaining items. Ids not in the list are ignored.
    fn take(list: &mut Vec<InboxItem>, ids: &[InboxItemId]) -> Vec<InboxItem> {
        let (taken, kept) = std::mem::take(list)
            .into_iter()
            .partition(|i| ids.contains(&i.id));
        *list = kept;
        taken
    }
}

/// Counters derived from the log. The driver checks its limits against these, so the limits
/// hold across quiesce, thaw and recovery.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    /// Turns started over the agent's life.
    pub turns: u64,
    /// Steps started over the agent's life.
    pub steps: u64,
    /// Steps started in the current turn; reset by `TurnStarted`.
    pub steps_this_turn: u32,
    /// Steps this turn that started after the model had already answered.
    pub forced_this_turn: u32,
    /// Failed model attempts in the current step; reset by `StepStarted`.
    pub failed_attempts_this_step: u32,
    /// Turns interrupted by a crash in a row; reset when a turn ends any other way.
    pub crash_resumes: u32,
    /// Prompt tokens over the agent's life, as providers reported them.
    pub prompt_tokens: u64,
    /// Completion tokens over the agent's life, as providers reported them.
    pub completion_tokens: u64,
}

/// A tool call the model made that has no committed result yet. After a crash, crash
/// repair writes a result for each one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    /// The call as the model made it, or as a `tool_pre` listener rewrote it.
    pub call: ToolCall,
    /// Whether its `ToolStarted` was logged, meaning the body may have run.
    pub started: bool,
}

/// Everything about an agent that is derived from its log.
///
/// Owned by the agent's [`crate::Journal`] while it runs, and carried whole in an image.
/// The fields are public for reading; changing them directly would make the view disagree
/// with the log, so only [`View::fold`] should.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// What the next model request is built from.
    pub history: History,
    /// Logged input waiting for a turn or step.
    pub inbox: Inbox,
    /// Counts the driver's limits are checked against.
    pub counters: Counters,
    /// The turn that has started and not ended, if any. Still set for a suspended turn.
    pub open_turn: Option<TurnId>,
    /// Whether a step has started and not ended.
    pub open_step: bool,
    /// Tool calls from the replies so far without a committed result, in model order.
    pub pending_calls: Vec<PendingCall>,
    /// The open turn was suspended by a drain at a step boundary and can continue as is.
    /// Set by `Quiesced`, kept through `Thawed` and `Recovered`, and cleared by any other
    /// event, since that means the agent has moved on.
    pub suspended: bool,
    /// The highest turn id started so far.
    last_turn: u64,
    /// Whether the last step that ended was answered or concluded. Lets `StepStarted` tell
    /// a forced continuation from a step the model was owed.
    last_step_answered: bool,
}

impl View {
    /// The id the next turn will get.
    pub fn next_turn_id(&self) -> TurnId {
        TurnId(self.last_turn + 1)
    }

    /// Applies one event. Deterministic, so live state and replayed state agree.
    ///
    /// Never fails: an event that names something the view doesn't have (an unknown call
    /// id, an inbox id already taken) changes nothing for that part.
    pub fn fold(&mut self, event: &Event) {
        if !matches!(
            event,
            Event::Thawed { .. } | Event::Recovered { .. } | Event::Quiesced { .. }
        ) {
            self.suspended = false;
        }
        match event {
            Event::SystemPrompt { source, text } => {
                match self.history.system.iter_mut().find(|(p, _)| p == source) {
                    Some(slot) => slot.1 = text.clone(),
                    None => self.history.system.push((source.clone(), text.clone())),
                }
            }
            Event::MessageReceived { message } => {
                self.inbox.last_logged = Some(message.id);
                self.inbox.push(
                    Source::Queue(message.id),
                    message.delivery,
                    message.body.clone(),
                );
            }
            Event::ContextQueued {
                source,
                delivery,
                text,
            } => {
                self.inbox.push(source.clone(), *delivery, text.clone());
            }
            Event::TurnStarted { turn, admitted } => {
                self.open_turn = Some(*turn);
                self.last_turn = self.last_turn.max(turn.0);
                self.counters.turns += 1;
                self.counters.steps_this_turn = 0;
                self.counters.forced_this_turn = 0;
                self.last_step_answered = false;
                for item in Inbox::take(&mut self.inbox.next_turn, admitted) {
                    self.history.messages.push(item.to_message());
                }
            }
            Event::StepStarted { admitted } => {
                if self.counters.steps_this_turn > 0 && self.last_step_answered {
                    self.counters.forced_this_turn += 1;
                }
                self.open_step = true;
                self.counters.steps += 1;
                self.counters.steps_this_turn += 1;
                self.counters.failed_attempts_this_step = 0;
                for item in Inbox::take(&mut self.inbox.next_step, admitted) {
                    self.history.messages.push(item.to_message());
                }
            }
            Event::AttemptFailed { .. } => self.counters.failed_attempts_this_step += 1,
            Event::AssistantReply { reply } => {
                if let Some(u) = reply.usage {
                    self.counters.prompt_tokens += u.prompt_tokens;
                    self.counters.completion_tokens += u.completion_tokens;
                }
                self.pending_calls
                    .extend(reply.tool_calls.iter().map(|c| PendingCall {
                        call: c.clone(),
                        started: false,
                    }));
                self.history.messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: reply.content.clone(),
                    tool_calls: reply.tool_calls.clone(),
                    tool_call_id: None,
                });
            }
            Event::ToolCallRewritten {
                call_id,
                name,
                arguments,
            } => {
                if let Some(p) = self
                    .pending_calls
                    .iter_mut()
                    .find(|p| &p.call.id == call_id)
                {
                    p.call.name = name.clone();
                    p.call.arguments = arguments.clone();
                }
            }
            Event::ToolStarted { call_id } => {
                if let Some(p) = self
                    .pending_calls
                    .iter_mut()
                    .find(|p| &p.call.id == call_id)
                {
                    p.started = true;
                }
            }
            Event::ToolResult { result } => {
                self.pending_calls.retain(|p| p.call.id != result.call_id);
                self.history.messages.push(ChatMessage {
                    role: Role::Tool,
                    content: result.content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(result.call_id.clone()),
                });
            }
            Event::StepEnded { outcome } => {
                self.open_step = false;
                self.last_step_answered =
                    matches!(outcome, StepOutcome::Answered | StepOutcome::Concluded);
            }
            Event::TurnEnded { end, .. } => {
                self.open_turn = None;
                self.open_step = false;
                match end {
                    TurnEnd::Interrupted => self.counters.crash_resumes += 1,
                    _ => self.counters.crash_resumes = 0,
                }
            }
            Event::Quiesced { suspended_turn, .. } => self.suspended = suspended_turn.is_some(),
            Event::ToolDenied { .. }
            | Event::ApprovalRequested { .. }
            | Event::ApprovalAnswered { .. }
            | Event::Thawed { .. }
            | Event::Recovered { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Finish;

    fn msg(id: u64, delivery: Delivery, body: &str) -> Event {
        Event::MessageReceived {
            message: QueuedMessage {
                id: QueueMsgId(id),
                sent_at_ms: 0,
                delivery,
                body: body.into(),
            },
        }
    }

    #[test]
    fn turn_admits_next_turn_items_and_step_admits_next_step_items() {
        let mut v = View::default();
        v.fold(&msg(1, Delivery::NextTurn, "hello"));
        v.fold(&msg(2, Delivery::NextStep, "also this"));
        assert_eq!(v.inbox.last_logged, Some(QueueMsgId(2)));
        let ids: Vec<_> = v.inbox.next_turn.iter().map(|i| i.id).collect();
        v.fold(&Event::TurnStarted {
            turn: v.next_turn_id(),
            admitted: ids,
        });
        let ids: Vec<_> = v.inbox.next_step.iter().map(|i| i.id).collect();
        v.fold(&Event::StepStarted { admitted: ids });
        let texts: Vec<_> = v
            .history
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(texts, ["hello", "also this"]);
        assert!(v.inbox.next_turn.is_empty() && v.inbox.next_step.is_empty());
        assert_eq!(v.open_turn, Some(TurnId(1)));
    }

    #[test]
    fn tool_calls_are_pending_until_their_result() {
        let mut v = View::default();
        let call = ToolCall {
            id: CallId::new("c1"),
            name: "clock".into(),
            arguments: "{}".into(),
        };
        v.fold(&Event::AssistantReply {
            reply: AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call.clone()],
                finish: Finish::ToolCalls,
                usage: None,
            },
        });
        v.fold(&Event::ToolCallRewritten {
            call_id: call.id.clone(),
            name: "clock".into(),
            arguments: r#"{"tz":"UTC"}"#.into(),
        });
        assert_eq!(v.pending_calls[0].call.arguments, r#"{"tz":"UTC"}"#);
        assert_eq!(v.history.messages[0].tool_calls[0].arguments, "{}");
        v.fold(&Event::ToolStarted {
            call_id: call.id.clone(),
        });
        assert!(v.pending_calls[0].started);
        v.fold(&Event::ToolResult {
            result: ToolResult::synthetic(&call, "x"),
        });
        assert!(v.pending_calls.is_empty());
    }

    #[test]
    fn a_step_after_an_answer_counts_as_forced() {
        let mut v = View::default();
        v.fold(&Event::TurnStarted {
            turn: TurnId(1),
            admitted: vec![],
        });
        v.fold(&Event::StepStarted { admitted: vec![] });
        v.fold(&Event::StepEnded {
            outcome: StepOutcome::ToolsRan,
        });
        v.fold(&Event::StepStarted { admitted: vec![] });
        assert_eq!(v.counters.forced_this_turn, 0);
        v.fold(&Event::StepEnded {
            outcome: StepOutcome::Answered,
        });
        v.fold(&Event::StepStarted { admitted: vec![] });
        assert_eq!(v.counters.forced_this_turn, 1);
    }

    #[test]
    fn suspension_survives_thaw_but_not_new_work() {
        let mut v = View::default();
        v.fold(&Event::TurnStarted {
            turn: TurnId(1),
            admitted: vec![],
        });
        v.fold(&Event::Quiesced {
            at: DrainPoint::Step,
            suspended_turn: Some(TurnId(1)),
            forced: false,
        });
        v.fold(&Event::Thawed {
            host: "h".into(),
            epoch: 2,
        });
        assert!(v.suspended);
        v.fold(&Event::StepStarted { admitted: vec![] });
        assert!(!v.suspended);
    }
}
