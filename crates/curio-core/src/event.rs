//! Session log events and the view folded from them.
//!
//! The [`View`] is everything about an agent that can be derived from its log: the
//! model-visible history, the inbox, counters and open work. It changes only by folding
//! events, both live (through [`crate::Journal::append`]) and during crash recovery, so a
//! recovered agent sees exactly what the live one saw.

use crate::agent::DrainPoint;
use crate::ids::{CallId, InboxItemId, PluginId, QueueMsgId, RequestId, TurnId};
use crate::message::{AssistantMessage, ChatMessage, Role, ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

/// When a queued message or context item should be used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Starts, or joins the start of, the next turn.
    #[default]
    NextTurn,
    /// Joins the next step of the running turn (steering), or the first step of the next turn.
    NextStep,
}

/// A message as the queue delivered it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: QueueMsgId,
    /// When the sender sent it. Logged for diagnosis; never used for ordering.
    pub sent_at_ms: u64,
    pub delivery: Delivery,
    pub body: String,
}

/// Where an inbox item came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Source {
    Queue(QueueMsgId),
    Plugin(PluginId),
    System,
}

/// Input that is logged and waiting to be used by a turn or step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: InboxItemId,
    pub source: Source,
    pub text: String,
}

impl InboxItem {
    /// How this item appears in the model-visible history.
    pub fn to_message(&self) -> ChatMessage {
        let text = match &self.source {
            Source::Queue(_) => self.text.clone(),
            Source::Plugin(p) => format!("[context from {p}]\n{}", self.text),
            Source::System => format!("[system notice]\n{}", self.text),
        };
        ChatMessage::text(Role::User, text)
    }
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepOutcome {
    /// Tools ran, so the model is owed another request.
    ToolsRan,
    /// The model answered without tool calls.
    Answered,
    /// A plugin ended the turn.
    Concluded,
    MaxTokens,
    Failed {
        error: String,
    },
    Cancelled,
    /// The process stopped during the step; written by crash repair.
    Interrupted,
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEnd {
    Completed,
    StepLimit,
    ContinuationLimit,
    MaxTokens,
    Blocked {
        reason: String,
    },
    Cancelled,
    Error {
        message: String,
    },
    /// The process stopped during the turn; written by crash repair.
    Interrupted,
    /// Drained at a step boundary. Never logged as a `TurnEnded`: the turn stays open and
    /// continues when the agent is thawed.
    Suspended,
}

/// One entry of an agent's session log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sets the system prompt contributed by one plugin, replacing its earlier one.
    SystemPrompt {
        source: PluginId,
        text: String,
    },
    /// A message pulled from the agent's queue. Logged before the queue is acknowledged.
    MessageReceived {
        message: QueuedMessage,
    },
    /// Context added by a plugin or by the runtime, waiting for a turn or step.
    ContextQueued {
        source: Source,
        delivery: Delivery,
        text: String,
    },
    /// A turn started with these next-turn items as its input.
    TurnStarted {
        turn: TurnId,
        admitted: Vec<InboxItemId>,
    },
    /// A step started with these next-step items added to the history. Never repeated on retry.
    StepStarted {
        admitted: Vec<InboxItemId>,
    },
    AttemptFailed {
        error: String,
    },
    AssistantReply {
        reply: AssistantMessage,
    },
    /// A `tool_pre` listener denied the call; the reason is the call's model-visible result.
    ToolDenied {
        call_id: CallId,
        reason: String,
    },
    ApprovalRequested {
        request: RequestId,
        call_id: CallId,
        question: String,
    },
    ApprovalAnswered {
        request: RequestId,
        approved: bool,
        note: Option<String>,
    },
    /// Written and synced before the tool body runs, so recovery can tell "may have run"
    /// from "never ran".
    ToolStarted {
        call_id: CallId,
    },
    ToolResult {
        result: ToolResult,
    },
    StepEnded {
        outcome: StepOutcome,
    },
    TurnEnded {
        turn: TurnId,
        end: TurnEnd,
    },
    Quiesced {
        at: DrainPoint,
        suspended_turn: Option<TurnId>,
        forced: bool,
    },
    Thawed {
        host: String,
        epoch: u64,
    },
    Recovered {
        host: String,
        epoch: u64,
        repaired_calls: usize,
    },
}

/// The model-visible conversation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    /// System prompt parts, one per contributing plugin, in first-contribution order.
    pub system: Vec<(PluginId, String)>,
    pub messages: Vec<ChatMessage>,
}

impl History {
    /// The messages a model request is built from: one system message, then the conversation.
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub next_turn: Vec<InboxItem>,
    pub next_step: Vec<InboxItem>,
    /// The newest queue message in the log; redelivered messages up to it are skipped.
    pub last_logged: Option<QueueMsgId>,
    next_item: u64,
}

impl Inbox {
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

    fn take(list: &mut Vec<InboxItem>, ids: &[InboxItemId]) -> Vec<InboxItem> {
        let (taken, kept) = std::mem::take(list)
            .into_iter()
            .partition(|i| ids.contains(&i.id));
        *list = kept;
        taken
    }
}

/// Counters derived from the log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    pub turns: u64,
    pub steps: u64,
    pub steps_this_turn: u32,
    /// Steps this turn that started after the model had already answered.
    pub forced_this_turn: u32,
    pub failed_attempts_this_step: u32,
    /// Turns interrupted by a crash in a row; reset when a turn completes.
    pub crash_resumes: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// A tool call the model made that has no committed result yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub call: ToolCall,
    pub started: bool,
}

/// Everything about an agent that is derived from its log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub history: History,
    pub inbox: Inbox,
    pub counters: Counters,
    pub open_turn: Option<TurnId>,
    pub open_step: bool,
    pub pending_calls: Vec<PendingCall>,
    /// The open turn was suspended by a drain at a step boundary and can continue as is.
    pub suspended: bool,
    last_turn: u64,
    last_step_answered: bool,
}

impl View {
    /// The id the next turn will get.
    pub fn next_turn_id(&self) -> TurnId {
        TurnId(self.last_turn + 1)
    }

    /// Applies one event. Deterministic, so live state and replayed state agree.
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
                    TurnEnd::Completed => self.counters.crash_resumes = 0,
                    _ => {}
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
