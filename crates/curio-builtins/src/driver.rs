//! `builtin.driver.react`: the default loop. Each turn is a series of steps; each step is
//! one model request (with retries) plus the tools that request asks for.

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

/// Loads [`ReactDriver`].
pub struct ReactDriverRunner;

/// The driver's code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverCode {
    /// Plugin id of the default model provider.
    pub model: PluginId,
    /// Hard cap on steps in one turn.
    pub max_steps_per_turn: u32,
    /// Hard cap on steps forced after the model already answered.
    pub max_forced_continuations: u32,
    /// How many parallel-safe tool calls may run at once.
    #[serde(default = "default_parallel_tools")]
    pub max_parallel_tools: usize,
    /// How long one tool body may run.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: u64,
    /// How many queue messages one pull may take.
    #[serde(default = "default_queue_batch")]
    pub queue_batch: usize,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn default_parallel_tools() -> usize {
    4
}
fn default_tool_timeout() -> u64 {
    300
}
fn default_queue_batch() -> usize {
    16
}

#[async_trait]
impl Runner for ReactDriverRunner {
    fn name(&self) -> &str {
        "builtin.driver.react"
    }

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

/// The default driver.
pub struct ReactDriver {
    code: DriverCode,
}

impl Plugin for ReactDriver {
    fn as_driver(&self) -> Option<&dyn Driver> {
        Some(self)
    }
}

/// A tool call after `tool_pre`: either runnable with parsed arguments, or already resolved.
enum Prepared {
    Run {
        call: ToolCall,
        args: Value,
        concurrency: Concurrency,
    },
    Done(ToolResult),
}

#[async_trait]
impl Driver for ReactDriver {
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

        loop {
            cx.apply_pending_commands().await?;
            if let Some(exit) = cx.settle_drain(DrainPoint::Turn).await? {
                return Ok(exit);
            }
            cx.phase = Phase::Idle;
            if cx.journal.view().inbox.next_turn.is_empty() {
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

        // 3. Commit in model order.
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
