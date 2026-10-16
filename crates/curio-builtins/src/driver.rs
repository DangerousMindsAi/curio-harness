//! `builtin.driver.react`: the default loop. Each turn is a series of steps; each step is
//! one model request (with retries) plus the tools that request asks for.
//!
//! The driver is a per-agent plugin that implements [`Driver`]. Every agent has exactly
//! one, and the runtime's agent task calls [`Driver::run`] once with the agent's
//! [`AgentCtx`]; replacing this plugin replaces the loop. It subscribes to no hooks. It
//! dispatches them: `agent_loaded`, `pre_step`, `request`, `request_error`, `response`,
//! `tool_pre`, `tool_post`, `turn_stopping` and `turn_ended` come from here, and
//! `drain_requested` and `snapshot` come from the [`AgentCtx`] methods it calls at safe
//! points. It has no state slot; everything it needs between steps is in the log's view.
//!
//! ```yaml
//! - id: driver
//!   runner: builtin.driver.react
//!   code:
//!     model: model                 # plugin id of the default model provider
//!     max_steps_per_turn: 40
//!     max_forced_continuations: 3
//!     max_parallel_tools: 4        # optional
//!     tool_timeout_secs: 120       # optional
//!     queue_batch: 16              # optional
//!     max_tokens: 4096             # optional
//!     temperature: 0.2             # optional
//! ```
//!
//! **Turns.** While idle, the driver waits on the agent's queue. A turn starts when the
//! inbox has next-turn input: the driver logs `TurnStarted`, which moves that input into
//! the history, and runs steps until something ends the turn. It then logs `TurnEnded`,
//! syncs, dispatches `turn_ended` and goes back to waiting. A turn ends when the model
//! answers and nobody asks for another step, a tool result concludes it, the reply hits
//! the token limit, `pre_step` rejects, the turn is cancelled, a model error isn't
//! retried, or a step or continuation cap is reached.
//!
//! **Steps.** Each step begins at a safe point, where the driver handles commands and may
//! quiesce. It then pulls steering messages without waiting, dispatches `pre_step` (where
//! plugins queue context), and logs `StepStarted`, which admits the next-step inbox into
//! the history exactly once. The request is built from the history alone, and retries
//! rebuild it the same way without admitting anything again. After the reply is logged and
//! `response` has run, a reply with tool calls goes through the tool pipeline; a reply
//! without them ends the step as answered. After an answer, the turn continues only if
//! something sits in the next-step inbox, either already or because a `turn_stopping`
//! listener queued it.
//!
//! **Tools.** The pipeline has three phases. Prepare runs `tool_pre` for each call in model
//! order, waits for approval where a listener asked for it, and parses the arguments;
//! denied, unknown or malformed calls get their result right away. Execute runs the
//! remaining calls in groups: a run of consecutive parallel-safe calls overlaps (up to
//! `max_parallel_tools`), and an exclusive call runs alone. Commit runs `tool_post` on each
//! result and logs it, again in model order, so the log never depends on which body
//! finished first.
//!
//! **Save points.** Appends are buffered by the log store; the driver forces them to
//! durable storage before each model request (so the request can be rebuilt exactly),
//! after the `ToolStarted` events of a group and before any of its bodies run (so recovery
//! can tell "might have run" from "never ran"), after asking for an approval, and at the end
//! of each turn. The queue and drain code in [`AgentCtx`] syncs after pulling messages and
//! when quiescing.
//!
//! **Cancellation and drains.** A cancel fires the turn's token. Model attempts, retry
//! waits, approval waits and tool bodies that can be interrupted all watch it, so the step ends soon after, and
//! calls that hadn't started get "not run" results. Every long wait goes through
//! [`AgentCtx::wait`] or [`AgentCtx::wait_one`], which keep handling commands meanwhile
//! and turn a passed drain deadline into a cancel. A drain at `step` quiesces at the next
//! step boundary and leaves the turn open in the log, and [`Driver::run`] continues that
//! turn first after a thaw.

use async_trait::async_trait;
use curio_core::{
    AgentCtx, Answer, AssistantMessage, Concurrency, DrainPoint, Driver, ErrorDecision, Event,
    Exit, Finish, Flow, HookEvent, LoadEnv, LoadedPlugin, ModelErrorKind, ModelRequest,
    Observation, Phase, Plugin, PluginId, RequestId, ResponseVerdict, Result, Route, Runner,
    StepOutcome, ToolCall, ToolDecision, ToolResult, TurnEnd, TurnId, dispatch,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Loads [`ReactDriver`]. Holds nothing itself.
pub struct ReactDriverRunner;

/// The driver's code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverCode {
    /// Plugin id of the default model provider. Required. It goes into each attempt's
    /// [`Route`], where a `request` listener may point it at another provider. Checked when
    /// [`Driver::run`] starts, since the plugin table doesn't exist yet at load.
    pub model: PluginId,
    /// Hard cap on steps in one turn, counted across a suspend and thaw. Required; when a
    /// step would exceed it, the turn ends with [`TurnEnd::StepLimit`].
    pub max_steps_per_turn: u32,
    /// Hard cap on steps forced after the model already answered, per turn. Required; 0
    /// means an answer always ends the turn, and reaching the cap ends it with
    /// [`TurnEnd::ContinuationLimit`].
    pub max_forced_continuations: u32,
    /// How many parallel-safe tool calls may run at once within one group. Defaults to 4;
    /// must be at least 1.
    #[serde(default = "default_parallel_tools")]
    pub max_parallel_tools: usize,
    /// How long one tool body may run, in seconds. Defaults to 300. A body that runs longer
    /// is dropped, and its result says the outcome is unknown.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: u64,
    /// How many queue messages one pull may take. Defaults to 16; must be at least 1.
    #[serde(default = "default_queue_batch")]
    pub queue_batch: usize,
    /// The output token limit sent with each request. Defaults to none, which leaves it
    /// to the provider. `request` listeners may change it per attempt.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// The sampling temperature sent with each request. Defaults to none, which leaves it
    /// to the provider. `request` listeners may change it per attempt.
    #[serde(default)]
    pub temperature: Option<f32>,
}

/// Default for [`DriverCode::max_parallel_tools`].
fn default_parallel_tools() -> usize {
    4
}
/// Default for [`DriverCode::tool_timeout_secs`].
fn default_tool_timeout() -> u64 {
    300
}
/// Default for [`DriverCode::queue_batch`].
fn default_queue_batch() -> usize {
    16
}

#[async_trait]
impl Runner for ReactDriverRunner {
    fn name(&self) -> &str {
        "builtin.driver.react"
    }

    /// Parses the code and checks that `max_parallel_tools` and `queue_batch` are at least
    /// 1. Starts with an empty state slot.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: DriverCode = env.parse_code(code)?;
        if code.max_parallel_tools == 0 || code.queue_batch == 0 {
            return Err(curio_core::Error::plugin(
                &env.plugin,
                "max_parallel_tools and queue_batch must be at least 1",
            ));
        }
        Ok(LoadedPlugin::stateless(Arc::new(ReactDriver { code })))
    }
}

/// The default driver: one agent's loop, configured by its [`DriverCode`].
///
/// Immutable like every plugin instance. All the loop's state lives in the [`AgentCtx`]
/// it's given, mostly in the journal's view, so the driver can stop at any safe point and
/// a new instance can pick up from an image or a recovered log.
pub struct ReactDriver {
    /// The validated code.
    code: DriverCode,
}

impl Plugin for ReactDriver {
    fn as_driver(&self) -> Option<&dyn Driver> {
        Some(self)
    }
}

/// A tool call after `tool_pre`: either runnable with parsed arguments, or already resolved.
///
/// The prepare phase builds one of these per call, in model order, and the execute and
/// commit phases keep that order by indexing into the list.
enum Prepared {
    /// Allowed, known and with valid JSON arguments: the body will run.
    Run {
        /// The call as `tool_pre` left it, possibly with rewritten arguments.
        call: ToolCall,
        /// `call.arguments` parsed as JSON (an empty string counts as `{}`).
        args: Value,
        /// The tool's concurrency, which decides how it's grouped.
        concurrency: Concurrency,
    },
    /// Resolved without running: denied, cancelled, an unknown tool or invalid arguments.
    /// Holds the synthetic result the model will see.
    Done(ToolResult),
}

#[async_trait]
impl Driver for ReactDriver {
    /// Runs the agent until it quiesces, or fails.
    ///
    /// First checks that `model` names a model provider, dispatches `agent_loaded` and
    /// syncs whatever its listeners logged. If the view has an open turn, which can
    /// only be one a drain suspended (recovery closes any other), the driver continues it
    /// before anything else. Then it loops: handle commands, quiesce if a drain is satisfied by a
    /// turn boundary, otherwise wait for next-turn input and run a turn.
    ///
    /// Returns [`Exit::Quiesced`] after a drain, either between turns or at a step
    /// boundary. Never returns [`Exit::Closed`]. Any error from a hook listener, the log
    /// (including [`curio_core::Error::Fenced`]) or the queue stops the agent with that
    /// error; the runtime then fails any drain waiters, and the agent can be recovered from
    /// its log.
    async fn run(&self, cx: &mut AgentCtx) -> Result<Exit> {
        cx.plugins.model(&self.code.model)?;
        dispatch(cx, &mut HookEvent::AgentLoaded { how: cx.loaded_by }).await?;
        cx.journal.sync().await?;

        if let Some(turn) = cx.journal.view().open_turn {
            // A turn suspended by a drain at a step boundary continues as it was.
            cx.observer.emit(Observation::TurnStarted { turn });
            cx.phase = Phase::Running;
            let end = self.run_turn(cx).await?;
            if end == TurnEnd::Suspended {
                return Ok(Exit::Quiesced);
            }
            self.finish_turn(cx, turn, end).await?;
        }

        // Safe point: between turns. Each pass handles commands, may quiesce, and either
        // starts a turn or waits for input.
        loop {
            cx.apply_pending_commands().await?;
            if let Some(exit) = cx.settle_drain(DrainPoint::Turn).await? {
                return Ok(exit);
            }
            cx.phase = Phase::Idle;
            if cx.journal.view().inbox.next_turn.is_empty() {
                // Returns early when a command arrives, and pulls nothing while a drain is
                // pending, so the loop gets back to the drain check above.
                cx.pull_messages(self.code.queue_batch, true).await?;
                continue;
            }

            let view = cx.journal.view();
            let turn = view.next_turn_id();
            let admitted = view.inbox.next_turn.iter().map(|i| i.id).collect();
            cx.begin_turn_scope();
            cx.journal.append(Event::TurnStarted { turn, admitted })?;
            cx.phase = Phase::Running;
            cx.observer.emit(Observation::TurnStarted { turn });
            let end = self.run_turn(cx).await?;
            if end == TurnEnd::Suspended {
                return Ok(Exit::Quiesced);
            }
            self.finish_turn(cx, turn, end).await?;
        }
    }
}

impl ReactDriver {
    /// Closes a turn: logs `TurnEnded` and syncs (the turn-boundary save point), dispatches
    /// `turn_ended`, syncs again for anything its listeners logged, and tells observers.
    /// Never called for [`TurnEnd::Suspended`], which leaves the turn open.
    async fn finish_turn(&self, cx: &mut AgentCtx, turn: TurnId, end: TurnEnd) -> Result<()> {
        cx.journal.append(Event::TurnEnded {
            turn,
            end: end.clone(),
        })?;
        cx.journal.sync().await?;
        dispatch(cx, &mut HookEvent::TurnEnded { turn, end: &end }).await?;
        cx.journal.sync().await?;
        cx.observer.emit(Observation::TurnEnded { turn, end });
        Ok(())
    }

    /// Runs steps until the turn ends. Returns [`TurnEnd::Suspended`] after quiescing at a
    /// step boundary, in which case the turn stays open in the log.
    ///
    /// The checks at the top of each step run in a fixed order: commands, drain, cancel,
    /// step cap. A step drain that arrives together with a cancel therefore suspends the
    /// turn instead of cancelling it, and a cancelled turn never starts another step. Each step's outcome is logged as `StepEnded` before
    /// the turn decides what follows. The continuation cap counts steps that started after
    /// an answer, so with `max_forced_continuations: 2` a turn can be pushed past an answer
    /// twice and ends on the third push.
    async fn run_turn(&self, cx: &mut AgentCtx) -> Result<TurnEnd> {
        loop {
            // Safe point: between steps.
            cx.apply_pending_commands().await?;
            if cx.settle_drain(DrainPoint::Step).await?.is_some() {
                return Ok(TurnEnd::Suspended);
            }
            if cx.turn_cancel.is_cancelled() {
                return Ok(TurnEnd::Cancelled);
            }
            if cx.journal.view().counters.steps_this_turn >= self.code.max_steps_per_turn {
                return Ok(TurnEnd::StepLimit);
            }

            cx.pull_messages(self.code.queue_batch, false).await?; // steering sent during the last step
            if let Flow::Reject(reason) = dispatch(cx, &mut HookEvent::PreStep).await? {
                return Ok(TurnEnd::Blocked { reason });
            }
            let admitted = cx
                .journal
                .view()
                .inbox
                .next_step
                .iter()
                .map(|i| i.id)
                .collect();
            cx.journal.append(Event::StepStarted { admitted })?;

            let outcome = self.run_step(cx).await?;
            cx.journal.append(Event::StepEnded {
                outcome: outcome.clone(),
            })?;
            match outcome {
                StepOutcome::ToolsRan => continue,
                StepOutcome::MaxTokens => return Ok(TurnEnd::MaxTokens),
                StepOutcome::Failed { error } => return Ok(TurnEnd::Error { message: error }),
                StepOutcome::Cancelled => return Ok(TurnEnd::Cancelled),
                StepOutcome::Interrupted => return Ok(TurnEnd::Interrupted),
                StepOutcome::Answered | StepOutcome::Concluded => {}
            }

            // Continuation is data: another step happens only if something is queued for it.
            if cx.journal.view().inbox.next_step.is_empty() {
                dispatch(cx, &mut HookEvent::TurnStopping).await?;
            }
            if cx.journal.view().inbox.next_step.is_empty() {
                return Ok(TurnEnd::Completed);
            }
            if cx.journal.view().counters.forced_this_turn >= self.code.max_forced_continuations {
                return Ok(TurnEnd::ContinuationLimit);
            }
        }
    }

    /// Runs one step after `StepStarted` is logged: model attempts until one succeeds or
    /// isn't retried, then the reply's tool calls.
    ///
    /// Each attempt dispatches `request` with a fresh [`Route`] from the code, builds the
    /// request from the history and the agent's tool specs, syncs (the save point before a
    /// model request) and calls the provider through [`AgentCtx::wait`] so commands still
    /// get through. A failed attempt is logged as `AttemptFailed`; `request_error` listeners
    /// then decide, and a retry waits out its delay unless the turn is cancelled.
    ///
    /// A reply cut off by the token limit loses its tool calls before it's logged, since
    /// truncated calls can't be trusted, and ends the step as `MaxTokens`. If a `response`
    /// listener stops the turn, every call gets a "not run" result and the step concludes.
    /// A cancel during an attempt or a retry wait returns [`StepOutcome::Cancelled`] without
    /// a logged reply.
    async fn run_step(&self, cx: &mut AgentCtx) -> Result<StepOutcome> {
        let mut reply = loop {
            let mut route = Route {
                provider: self.code.model.clone(),
                max_tokens: self.code.max_tokens,
                temperature: self.code.temperature,
            };
            dispatch(cx, &mut HookEvent::Request { route: &mut route }).await?;
            let provider = cx.plugins.model(&route.provider)?;
            let request = ModelRequest {
                messages: cx.journal.view().history.to_request_messages(),
                tools: cx.plugins.tool_specs(),
                route,
            };
            cx.journal.sync().await?; // save point: before the model request

            let (observer, cancel) = (cx.observer.clone(), cx.turn_cancel.clone());
            let attempt = cx
                .wait(async move {
                    let model = provider.as_model().expect("checked by PluginTable::model");
                    model.complete(request, observer, cancel).await
                })
                .await;
            let error = match attempt {
                Ok(reply) => break reply,
                Err(e) if e.kind == ModelErrorKind::Cancelled => return Ok(StepOutcome::Cancelled),
                Err(e) => e,
            };
            cx.journal.append(Event::AttemptFailed {
                error: error.to_string(),
            })?;
            if cx.turn_cancel.is_cancelled() {
                return Ok(StepOutcome::Cancelled);
            }
            let mut decision = ErrorDecision::Fail;
            dispatch(
                cx,
                &mut HookEvent::RequestError {
                    error: &error,
                    decision: &mut decision,
                },
            )
            .await?;
            match decision {
                ErrorDecision::Fail => {
                    return Ok(StepOutcome::Failed {
                        error: error.to_string(),
                    });
                }
                ErrorDecision::Retry { after } => {
                    // A cancel during this wait ends it early; the next attempt then sees
                    // the cancelled token and returns at once.
                    cx.observer.emit(Observation::Retrying {
                        error: error.to_string(),
                    });
                    let cancel = cx.turn_cancel.clone();
                    cx.wait(async move {
                        tokio::select! {
                            _ = tokio::time::sleep(after) => {}
                            _ = cancel.cancelled() => {}
                        }
                    })
                    .await;
                }
            }
        };

        if reply.finish == Finish::Length {
            reply.tool_calls.clear(); // calls cut off by the token limit cannot be trusted
        }
        cx.journal.append(Event::AssistantReply {
            reply: reply.clone(),
        })?;
        let mut verdict = ResponseVerdict::Proceed;
        dispatch(
            cx,
            &mut HookEvent::Response {
                reply: &reply,
                verdict: &mut verdict,
            },
        )
        .await?;

        if reply.finish == Finish::Length {
            return Ok(StepOutcome::MaxTokens);
        }
        if verdict == ResponseVerdict::StopTurn {
            for call in &reply.tool_calls {
                let result =
                    ToolResult::synthetic(call, "Not run: the turn was stopped before tools ran.");
                cx.journal.append(Event::ToolResult { result })?;
            }
            return Ok(StepOutcome::Concluded);
        }
        if reply.tool_calls.is_empty() {
            return Ok(StepOutcome::Answered);
        }
        self.run_tools(cx, reply).await
    }

    /// Prepares calls one at a time in model order, runs them (parallel-safe calls
    /// overlap), and commits results in model order.
    ///
    /// Every call in `reply` ends with exactly one logged `ToolResult`. A body only runs
    /// after its `ToolStarted` is synced, and a group that would start after a cancel gets
    /// "not run" results instead. Results of bodies that did run are committed even when the
    /// turn was cancelled meanwhile; the step then reports [`StepOutcome::Cancelled`].
    /// Otherwise it's [`StepOutcome::Concluded`] when a `tool_post` listener marked any
    /// result as concluding the turn, and [`StepOutcome::ToolsRan`] when none did.
    async fn run_tools(&self, cx: &mut AgentCtx, reply: AssistantMessage) -> Result<StepOutcome> {
        // 1. Prepare sequentially: decide, ask, validate.
        let mut prepared = Vec::with_capacity(reply.tool_calls.len());
        for mut call in reply.tool_calls {
            if cx.turn_cancel.is_cancelled() {
                prepared.push(Prepared::Done(ToolResult::synthetic(
                    &call,
                    "Not run: the turn was cancelled.",
                )));
                continue;
            }
            let mut decision = ToolDecision::Allow;
            dispatch(
                cx,
                &mut HookEvent::ToolPre {
                    call: &mut call,
                    decision: &mut decision,
                },
            )
            .await?;
            if let ToolDecision::Ask { question } = decision {
                decision = self.ask(cx, &call, question).await?;
            }
            prepared.push(match decision {
                ToolDecision::Allow => prepare_call(cx, call),
                ToolDecision::Deny { reason } => {
                    cx.journal.append(Event::ToolDenied {
                        call_id: call.id.clone(),
                        reason: reason.clone(),
                    })?;
                    Prepared::Done(ToolResult::synthetic(&call, reason))
                }
                ToolDecision::Ask { .. } => unreachable!("ask returns Allow or Deny"),
            });
        }

        // 2. Execute: consecutive parallel-safe calls form one group; exclusive calls run alone.
        // A resolved call between two parallel ones ends the group, so the order of bodies
        // never jumps past a call the model listed earlier.
        let mut results: Vec<Option<ToolResult>> = prepared
            .iter()
            .map(|p| match p {
                Prepared::Done(r) => Some(r.clone()),
                Prepared::Run { .. } => None,
            })
            .collect();
        let mut index = 0;
        while index < prepared.len() {
            let Prepared::Run { concurrency, .. } = &prepared[index] else {
                index += 1;
                continue;
            };
            let mut group = vec![index];
            if *concurrency == Concurrency::Parallel {
                while let Some(Prepared::Run {
                    concurrency: Concurrency::Parallel,
                    ..
                }) = prepared.get(index + group.len())
                {
                    group.push(index + group.len());
                }
            }
            index += group.len();

            let calls: Vec<(usize, &ToolCall, &Value)> = group
                .iter()
                .map(|&i| match &prepared[i] {
                    Prepared::Run { call, args, .. } => (i, call, args),
                    Prepared::Done(_) => unreachable!("groups hold only runnable calls"),
                })
                .collect();
            if cx.turn_cancel.is_cancelled() {
                for (i, call, _) in calls {
                    results[i] = Some(ToolResult::synthetic(
                        call,
                        "Not run: the turn was cancelled.",
                    ));
                }
                continue;
            }
            for (_, call, _) in &calls {
                cx.journal.append(Event::ToolStarted {
                    call_id: call.id.clone(),
                })?;
            }
            cx.journal.sync().await?; // save point: "started" is durable before any body runs
            for (_, call, _) in &calls {
                cx.observer.emit(Observation::ToolCalled {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
            }

            // Each body gets its own timeout. Dropping a timed-out future is how it's
            // stopped; the call's cancel token isn't fired.
            let timeout = Duration::from_secs(self.code.tool_timeout_secs);
            let futures: Vec<_> = calls
                .iter()
                .map(|&(i, call, args)| {
                    let entry = cx.plugins.tool(&call.name).expect("checked in prepare_call");
                    let plugin = entry.plugin.clone();
                    let tcx = cx.tool_cx(entry, call.id.clone());
                    let args = args.clone();
                    async move {
                        let tool = plugin.as_tool().expect("indexed as a tool");
                        let output = match tokio::time::timeout(timeout, tool.call(tcx, args)).await {
                            Ok(output) => output,
                            Err(_) => curio_core::ToolOutput::error(format!(
                                "Timed out after {} seconds; the tool was stopped and its outcome is unknown.",
                                timeout.as_secs()
                            )),
                        };
                        (i, output)
                    }
                })
                .collect();
            let max_parallel = self.code.max_parallel_tools;
            let outputs = cx
                .wait(
                    futures::stream::iter(futures)
                        .buffer_unordered(max_parallel)
                        .collect::<Vec<_>>(),
                )
                .await;
            for (i, output) in outputs {
                let Prepared::Run { call, .. } = &prepared[i] else {
                    unreachable!()
                };
                results[i] = Some(ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: output.content,
                    is_error: output.is_error,
                    concludes_turn: false,
                });
            }
        }

        // 3. Commit in model order. Observers see a result after `tool_post` has run on it,
        // which is the version the model will see.
        let mut concluded = false;
        for result in results {
            let mut result = result.expect("every call has a result");
            dispatch(
                cx,
                &mut HookEvent::ToolPost {
                    result: &mut result,
                },
            )
            .await?;
            concluded |= result.concludes_turn;
            cx.observer.emit(Observation::ToolFinished {
                call_id: result.call_id.clone(),
                name: result.name.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            });
            cx.journal.append(Event::ToolResult { result })?;
        }

        if cx.turn_cancel.is_cancelled() {
            return Ok(StepOutcome::Cancelled);
        }
        Ok(if concluded {
            StepOutcome::Concluded
        } else {
            StepOutcome::ToolsRan
        })
    }

    /// Asks the handle's holder about a call and waits for the answer, handling other
    /// commands meanwhile. A cancelled turn counts as a denial.
    ///
    /// The request id is `approve-<call id>`. `ApprovalRequested` is logged and synced
    /// before observers hear the question, so the question is durable before anyone can
    /// answer it. The agent stays in memory while it waits; there's no parking. A drain
    /// that arrives meanwhile waits until its deadline, which cancels the turn and so
    /// denies the call. Returns only `Allow` or `Deny`; a denial carries the user's note
    /// when there is one. A cancelled wait logs no `ApprovalAnswered`.
    async fn ask(
        &self,
        cx: &mut AgentCtx,
        call: &ToolCall,
        question: String,
    ) -> Result<ToolDecision> {
        let request = RequestId::new(format!("approve-{}", call.id));
        cx.journal.append(Event::ApprovalRequested {
            request: request.clone(),
            call_id: call.id.clone(),
            question: question.clone(),
        })?;
        cx.journal.sync().await?;
        cx.observer.emit(Observation::ApprovalRequested {
            request: request.clone(),
            tool: call.name.clone(),
            arguments: call.arguments.clone(),
            question,
        });
        let answer = loop {
            if let Some(answer) = cx.answers.remove(&request) {
                break Some(answer);
            }
            if cx.turn_cancel.is_cancelled() {
                break None;
            }
            // Answers arrive as commands, which `wait_one` handles before returning, so the
            // future here only has to end the wait on a cancel.
            let cancelled = cx.turn_cancel.clone().cancelled_owned();
            tokio::pin!(cancelled);
            cx.wait_one(cancelled).await;
        };
        let Some(Answer { approve, note }) = answer else {
            return Ok(ToolDecision::Deny {
                reason: "Not run: the turn was cancelled while waiting for approval.".into(),
            });
        };
        cx.journal.append(Event::ApprovalAnswered {
            request,
            approved: approve,
            note: note.clone(),
        })?;
        Ok(if approve {
            ToolDecision::Allow
        } else {
            let reason = match note {
                Some(note) => format!("The user denied this call: {note}"),
                None => "The user denied this call.".to_string(),
            };
            ToolDecision::Deny { reason }
        })
    }
}

/// Resolves an allowed call into something runnable, or into a synthetic error result.
///
/// Looks the tool up by name and parses the arguments, treating empty or blank text as
/// `{}` since some models send that for tools without parameters. An unknown tool or
/// arguments that aren't valid JSON give a model-visible error and no body run. Argument
/// schemas aren't checked here; each tool validates its own arguments.
fn prepare_call(cx: &AgentCtx, call: ToolCall) -> Prepared {
    let Some(entry) = cx.plugins.tool(&call.name) else {
        let text = format!("Unknown tool `{}`.", call.name);
        return Prepared::Done(ToolResult::synthetic(&call, text));
    };
    let concurrency = entry
        .plugin
        .as_tool()
        .map(|t| t.spec().concurrency)
        .unwrap_or(Concurrency::Exclusive);
    let raw = if call.arguments.trim().is_empty() {
        "{}"
    } else {
        call.arguments.as_str()
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(args) => Prepared::Run {
            call,
            args,
            concurrency,
        },
        Err(e) => {
            let text = format!("Not run: the arguments are not valid JSON ({e}).");
            Prepared::Done(ToolResult::synthetic(&call, text))
        }
    }
}
