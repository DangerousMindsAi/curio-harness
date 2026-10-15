# Agent loop proposal

*A plugin-first agent loop for curio-harness, with checkpoint, resume and graceful quiesce, for many isolated agents in one process.*

Status: proposal for discussion. Nothing here is implemented.

This builds on the [architecture survey](../harness-survey/README.md), the [agent loop docs](../agent-loops/README.md) and the [crash recovery doc](../crash-recovery/README.md). Where an idea comes from one of the three surveyed harnesses, the text says which.

**About the pseudo-code.** It is Rust-flavored: plugin interfaces are written as Rust traits, and the loop is written as `async fn`s over an explicit context. It is not meant to compile. Lifetimes, error plumbing and most generics are left out, and `async fn` appears in traits even where a real implementation would need boxed futures for `dyn` use. Rust was chosen because curio-harness will be written in it, and two of the requirements (strict per-agent isolation and an explicit context) are easier to reason about when the borrow checker is part of the picture.

## Contents

- [Summary](#summary)
- [Requirements and where each is met](#requirements-and-where-each-is-met)
- [Vocabulary](#vocabulary)
- [Process layout](#process-layout)
- [The agent context](#the-agent-context)
- [Runners and code](#runners-and-code)
- [Hooks](#hooks)
- [The loop](#the-loop)
- [Checkpoint and resume](#checkpoint-and-resume)
- [Quiesce and graceful shutdown](#quiesce-and-graceful-shutdown)
- [Inbound messages](#inbound-messages)
- [Migrating an agent between hosts](#migrating-an-agent-between-hosts)
- [Isolation in a multi-tenant process](#isolation-in-a-multi-tenant-process)
- [Worked example](#worked-example)
- [Decisions and open questions](#decisions-and-open-questions)
- [What this borrows](#what-this-borrows)

## Summary

The process hosts a `Runtime` that holds only shared, immutable machinery: a registry of runners, storage backends and a scheduler. Each agent is one async task that exclusively owns an `AgentCtx`, and every piece of per-agent state lives in that struct. Nothing per-agent is reachable through a global, a static, or a task-local.

Every behavior is a plugin, including the loop driver, the model provider, the session store and each tool. A plugin is a pair: a **runner**, which is Rust code compiled into the host and implementing the `Runner` trait, and **code**, which is whatever the runner interprets. For a built-in runner the code is a block of configuration; for a WASM or Python runner it is a whole program. Runners are shared by every agent in the process and hold no agent state. Loading code through a runner produces a plugin instance that is immutable after load; its mutable state lives in a typed slot in the agent's context. This split is what makes checkpointing uniform and isolation checkable: the harness can serialize every slot without trusting each plugin to remember to, and a plugin has nowhere else to keep state.

The default driver is a small loop in the style of DeepSeek Harness. Each turn is a series of steps; each step is one model request (with retries) plus the tools that request asks for. Plugins intercept it at a short list of typed hooks. Every input the model sees is first appended to the agent's session log, and each request is rebuilt from that log, so the log is enough to resume.

Checkpointing is event-sourced. The log is forced to disk before every model request and before every tool body, and a snapshot of plugin state slots is written at step boundaries. Resume rebuilds the context from the recorded configuration, restores the last snapshot, replays the events after it, and patches any loose ends the same way DeepSeek does.

Quiesce is a first-class phase. A drain request stops new work from being admitted, lets the agent reach a chosen safe point (end of step or end of turn), writes a complete snapshot and a `Quiesced` marker, and releases the session. A quiesced agent resumes exactly where it stopped, from the snapshot alone, with no replay, no repair and no user message needed. None of the three surveyed harnesses can do this.

Quiesce is also how agents move between hosts when a cluster is resized. Every inbound message carries a sender-assigned timestamp and a checksum; the agent keeps pending messages sorted by that pair and drops duplicates, so messages that arrive while the agent is quiesced, arrive at both the old and the new host, or are forwarded twice are all admitted exactly once and in the same order. A fencing epoch on the session store guarantees that only one host can write to the agent at a time. The move is invisible to senders.

## Requirements and where each is met

| Requirement | Where it is met |
|---|---|
| Everything is a plugin | The driver, model, store, tools and all policy are plugins behind service kinds and hooks ([Runners and code](#runners-and-code), [Hooks](#hooks)). Swapping the driver swaps the loop. |
| Written in Rust | Runner and plugin interfaces are Rust traits; the agent is a single-owner task, so the context needs no locks ([The agent context](#the-agent-context)). |
| Checkpoint and resume | Write-ahead save points, snapshots of state slots at step boundaries, and a resume procedure that replays and repairs ([Checkpoint and resume](#checkpoint-and-resume)). Graceful quiesce is covered in its own section. |
| Many agents per process, configured independently, strictly isolated, all state in a context variable | One `AgentCtx` per agent holding all mutable state; immutable plugin instances; per-agent capability grants and quotas; isolation level chosen per runner kind ([Isolation](#isolation-in-a-multi-tenant-process)). |
| Plugins are a runner plus code | `Runner::load(code) -> Plugin` with code being configuration, a WASM component, or an external program ([Runners and code](#runners-and-code)). |
| Move agents between hosts transparently when resizing a cluster | Quiesce, thaw on the new host, forward messages from the old host, deduplicate and sort, then resume ([Inbound messages](#inbound-messages), [Migrating an agent between hosts](#migrating-an-agent-between-hosts)). |

## Vocabulary

- **Runtime**: the per-process host. Holds the runner registry, storage backends and the scheduler. Shared by all agents, never mutated by them.
- **Agent**: one configured conversation partner with its own session log. Owned by exactly one task.
- **AgentCtx**: the struct that holds all of one agent's mutable state. The "context variable" from the requirements.
- **Runner**: a Rust type implementing `Runner`, registered once per process under a kind name such as `builtin.compaction` or `wasm`.
- **Code**: what a runner loads for one plugin: an inline config table, a file, or a versioned package, always pinned by digest.
- **Plugin instance**: what `Runner::load` returns for one agent. Immutable after load.
- **State slot**: the mutable, serializable state of one plugin instance, stored in `AgentCtx`.
- **Service**: a capability with exactly one provider per agent (`Driver`, `Model`, `SessionStore`, `Approval`) or many (`Tool(name)`).
- **Hook**: a typed interception point the driver or tool pipeline dispatches to every subscribed plugin, in order.
- **Turn / step / attempt**: as in DeepSeek. A turn answers one input; a step is one model request plus its tools; an attempt is one try at the request.
- **Safe point**: a moment where the agent's state is consistent and can be snapshotted or quiesced: before a step starts, or after a turn ends.
- **Envelope**: the wrapper on every inbound message, carrying a sender-assigned timestamp and a checksum. The pair `(timestamp, checksum)` is the message's sort key, and the checksum is its identity for deduplication.
- **Spool**: a durable, append-only file of envelopes kept by a host for an agent that is not running there. Appending needs no lease.
- **Epoch**: a counter on the agent's session store that increases each time a host takes ownership. Writes carrying an older epoch are refused.
- **Thaw**: loading a quiesced agent's snapshot on a host and taking ownership, without starting to run it.

## Process layout

```text
process
└── Runtime (shared, immutable after start)
    ├── RunnerRegistry: kind -> Arc<dyn Runner>
    ├── StoreBackends:  where session logs live (files, SQLite, object store)
    ├── Scheduler:      per-tenant quotas, admission, fair sharing of model/tool slots
    ├── Ingress:        receives envelopes; routes to a live agent or appends to its spool
    ├── Spools:         AgentId -> durable spool, for agents quiesced on or moving off this host
    └── agents: AgentId -> AgentHandle      (a channel sender, no state)

agent task (one per live agent, owns everything below)
└── AgentCtx
    ├── config          pinned in the log at creation
    ├── plugins         immutable instances, in hook order, plus a service index
    ├── state           one slot per plugin
    ├── log             append-only session log (via the SessionStore plugin)
    ├── inbox           pending envelopes sorted by (timestamp, checksum), seen set, next_turn / next_step
    ├── phase           Idle | Running | Draining | Parked | Quiesced | Thawed | Closed
    ├── counters        steps, turns, forced continuations, tokens
    ├── grants          what this agent may touch (fs roots, hosts, secrets, services)
    └── cancel, quota   cancellation token, tenant quota handle
```

Outside callers talk to an agent only through its `AgentHandle`:

```rust
pub enum Command {
    Send { envelope: Envelope },                     // see Inbound messages; delivery is inside
    Merge { envelopes: Vec<Envelope> },              // a batch forwarded from another host's spool
    Cancel { reason: CancelReason },                 // abort the current turn
    Drain { at: DrainPoint, deadline: Instant, reply: oneshot::Sender<DrainResult> },
    Answer { request: RequestId, answer: Answer },   // approval or question from a plugin
    Reconfigure { patch: ConfigPatch },              // applied at the next safe point
    Release,                                         // leave Thawed and start running
}

pub struct AgentHandle { tx: mpsc::Sender<Command>, id: AgentId, tenant: TenantId }
```

The agent task is an actor. Commands are drained at defined points in the loop, so nothing outside the task ever holds a reference into `AgentCtx`.

## The agent context

```rust
/// All mutable state for one agent. Owned by the agent's task; never shared.
pub struct AgentCtx {
    pub id: AgentId,
    pub tenant: TenantId,
    config: Arc<AgentConfig>,        // as recorded in the log; changes only via Reconfigure
    plugins: PluginTable,            // Box<dyn Plugin> in hook order + service index
    state: StateSlots,               // PluginId -> Box<dyn PluginState>
    log: SessionLog,                 // append, sync, read; backed by the SessionStore service
    inbox: Inbox,                    // sorted, deduplicated pending set; admission is a log event
    phase: Phase,
    counters: Counters,              // derived from the log; cached here
    grants: Grants,                  // capabilities granted by config, per plugin
    commands: mpsc::Receiver<Command>,
    cancel: CancellationToken,       // child token per turn
    quota: QuotaHandle,              // tenant limits; acquiring may wait, never panics
}

pub enum Phase {
    Idle,
    Running { turn: TurnId },
    Draining { at: DrainPoint, deadline: Instant, waiters: Vec<oneshot::Sender<DrainResult>> },
    Parked { waiting_on: RequestId },  // e.g. a human approval; may be evicted from memory
    Quiesced,                          // snapshot written, lease released; task has exited
    Thawed { epoch: Epoch },           // loaded and owned on this host, accepting messages, not running
    Closed,
}

/// Mutable state of one plugin. Serialized into snapshots; restored on resume.
pub trait PluginState: Send + erased_serde::Serialize + Any {}
```

Three properties follow from this layout.

1. **The context is a parameter, never ambient.** Every hook receives a borrow of the context (or a narrower view of it), so a plugin that tries to reach another agent's state has nothing to name. A `tokio::task_local!` would look tidier, but it silently disappears in tasks spawned for tool bodies and makes cross-agent leaks a runtime question instead of a compile-time one.
2. **One owner, no locks.** The agent task holds `&mut AgentCtx` for its whole life. Concurrency inside an agent (parallel tool bodies) gets a read-only `ToolCx` built from the context, and any change those bodies want to make comes back as data in their results.
3. **The context is reconstructible.** `config` and the plugin table are rebuilt from the log header; `state`, `inbox` and `counters` are rebuilt from the last snapshot plus the events after it. Nothing else needs saving.

## Runners and code

```rust
/// Compiled into the host. Registered once per process; shared by every agent.
/// Holds only immutable, tenant-neutral resources (a WASM engine, a parser).
pub trait Runner: Send + Sync + 'static {
    /// Kind name used in agent config, e.g. "builtin.react-driver", "wasm", "process.python".
    fn kind(&self) -> RunnerKind;

    /// Version of the state-slot format this runner writes. Bumped on incompatible change.
    fn state_version(&self) -> u32;

    /// Validate and load `code` for one agent. Misconfiguration fails here, at creation
    /// or resume, never later in the loop.
    async fn load(&self, code: &Code, grants: &PluginGrants, env: &LoadEnv)
        -> Result<LoadedPlugin, LoadError>;

    /// Upgrade a state slot written by an older version of this runner.
    fn migrate(&self, blob: StateBlob, from: u32) -> Result<StateBlob, MigrateError> {
        Err(MigrateError::Unsupported { from, to: self.state_version() })
    }
}

pub enum Code {
    Inline(toml::Table),                                    // config-only plugins
    File { path: PathBuf, digest: Digest },                 // a script or module on disk
    Package { name: String, version: Version, digest: Digest }, // fetched and cached by the runtime
}

pub struct LoadedPlugin {
    pub instance: Box<dyn Plugin>,
    pub initial_state: Box<dyn PluginState>,
}

/// One plugin for one agent. Immutable after load: hooks take `&self`.
pub trait Plugin: Send + Sync {
    fn id(&self) -> &PluginId;

    /// Hooks this plugin subscribes to, with an ordering key.
    fn subscriptions(&self) -> &[Subscription];

    /// Services this plugin provides, e.g. [Service::Model] or [Service::Tool("bash")].
    fn provides(&self) -> &[Service];

    /// Handle one hook. `cx` exposes this plugin's own state slot mutably,
    /// and the rest of the agent only through the hook's view.
    async fn on_hook(&self, cx: &mut HookCx<'_>, event: &mut HookEvent<'_>)
        -> Result<Flow, PluginError>;

    /// Decode a snapshot blob back into state. Default: serde with the runner's format.
    fn decode_state(&self, blob: StateBlob) -> Result<Box<dyn PluginState>, PluginError>;
}
```

Service traits are separate from hooks, because a service is called rather than notified:

```rust
pub trait Driver: Plugin {
    /// Run the agent until it quiesces or closes. The default implementation is
    /// the loop in the next section; replacing this plugin replaces the loop.
    async fn run(&self, cx: &mut AgentCtx) -> Result<Exit, DriverError>;
}

pub trait ModelProvider: Plugin {
    async fn stream(&self, cx: &RequestCx<'_>, req: ModelRequest)
        -> Result<ModelStream, ModelError>;
}

pub trait Tool: Plugin {
    fn spec(&self) -> &ToolSpec;                  // name, schema, concurrency: Parallel | Exclusive
    async fn call(&self, cx: ToolCx, args: Value) -> ToolResult;
}

pub trait SessionStore: Plugin {
    async fn open(&self, id: &SessionId, lease: LeaseMode) -> Result<StoreHandle, StoreError>;
}
```

### Runner kinds

The runner kind decides what "code" means and how strongly that code is isolated. Proposed initial set:

| Runner kind | Code | Where the code runs | Isolation |
|---|---|---|---|
| `builtin.*` (one runner per feature: `builtin.react-driver`, `builtin.compaction`, `builtin.tool.bash`, `builtin.model.openai-compat`, ...) | Inline TOML table, validated against the runner's config schema | In the host, as compiled Rust | Trusted code; isolation comes from the context and grants |
| `wasm` | A WASM component implementing a WIT world that mirrors the `Plugin` hooks | In the host, one WASM instance per plugin per agent | Separate linear memory, fuel and memory limits per instance, host calls only through grants |
| `process` (`process.python`, `process.node`, ...) | A package or directory with an entry point | A child process per agent, speaking the hook protocol over JSON-RPC on stdio | OS process boundary, plus the sandbox the `process` runner applies |
| `mcp` | An MCP server spec | Wherever the MCP server runs | Tools only; each MCP tool becomes a `Tool` service |

A "configuration file" plugin and an "entire application in a higher-level language" plugin are then the same thing to the loop: both are `Box<dyn Plugin>` with subscriptions. The `process` runner translates `on_hook` into a request to the child and the reply into a `Flow`; the hook payload crosses the boundary as JSON, which is also what the session log stores.

A Rust plugin implements the `Plugin` trait directly, but a plugin written in Python or compiled to WASM cannot see Rust traits. Each of those runners therefore needs the hook list, payload types and `Flow` results written down in a language-neutral form: a WIT world for WASM components, and a JSON-RPC schema for processes. The decision is to define the hooks once, as WIT, and generate the JSON-RPC schema and the Rust bindings from it, so that adding a hook or a payload field cannot leave one runner behind the others.

## Hooks

Each hook has a typed payload that listeners may mutate in place, and every listener returns a `Flow`:

```rust
pub enum Flow {
    Continue,            // pass the (possibly mutated) payload to the next listener
    Stop,                // short-circuit: later listeners do not run
    Reject(Reason),      // only where the hook allows it, e.g. pre_step, tool_pre
}
```

Listeners run in ascending `Subscription::order`, with ties broken by their position in the agent config. This replaces DeepSeek's `next()`-style waterfall, where a listener that forgets to call `next()` silently ends the chain. Here continuing is the default and stopping is explicit. Async closures that call `next()` are also awkward to express with Rust lifetimes.

| Hook | Dispatched | Payload a listener may change | Typical plugins |
|---|---|---|---|
| `agent_loaded` | After create or resume, before any work | May enqueue context into `next_step` | Instructions loader, session-start hooks |
| `pre_step` | At the start of each step, after claiming `next_step` input | Add context messages, compact history, `Reject` (turn ends blocked) | Compaction, time context, skills, plan mode, goal driver |
| `request` | Before each attempt | Model route, effort, max tokens, tool choice | Model selection, cost router |
| `request_error` | After a failed attempt | Set `decision = Retry` only after proving progress (for example, compaction changed the history) | Retry with backoff, overflow compaction, image offload |
| `response` | After the reply is logged, before tools run | Mark the step as stalled or stop the turn | Stationarity detector, refusal handling |
| `tool_pre` | Per call, sequentially in model order | Allow, deny with a model-visible reason, ask a human, rewrite arguments (logged) | Permission policy, hooks bridges, auto-review |
| `tool_execute` | Wraps each tool body; may run concurrently | Wrap the future (timeout, sandbox); read-only `ToolCx` | Timeout, sandbox, metering |
| `tool_post` | Per result, sequentially in model order | Rewrite the result, attach context for the next step, mark the turn concluded | Spill, repeat reminders |
| `turn_stopping` | When nothing is owed and `next_step` is empty | Enqueue into `next_step` to force another step | Stop hooks, todo gate, goal verification |
| `turn_ended` | After the turn's end record is written | Enqueue into `next_turn` | Goal round driver, notifications |
| `snapshot` | At a snapshot safe point | Contribute opaque state (only plugins whose state is not in a slot, such as `process` plugins) | Python and WASM runners |
| `replay` | For each event after the snapshot during resume | Rebuild derived state in the slot; no side effects allowed | Retry counters, repeat counters |
| `drain_requested` | When a drain is requested | Stop admitting background work; report what must finish | Background jobs, subagents, schedulers |

Two rules hold for every hook:

1. **Model-visible implies logged.** A hook that changes what the model will see does so by appending an event through `HookCx::append`. The request builder reads only the log, so a hook cannot inject anything that a resumed agent would not also see.
2. **Hooks that run concurrently get no mutable state.** `tool_execute` is the only hook that can run in parallel with itself, so it receives `&ToolCx`, not `&mut HookCx`. Anything it learns returns inside the `ToolResult` and is folded into state by `tool_post`, which runs in model order.

## The loop

This is the default `Driver` plugin, `builtin.react-driver`. Lines marked `// [hook]` dispatch to plugins and lines marked `// [config]` read the driver's code table.

### Driving the agent

```rust
async fn run(&self, cx: &mut AgentCtx) -> Result<Exit, DriverError> {
    dispatch(cx, Hook::AgentLoaded).await?;                       // [hook] agent_loaded
    loop {
        cx.apply_pending_commands()?;          // Send, Merge, Cancel, Drain, Answer, Reconfigure, Release

        // Thawed: owned on this host and accepting messages, but not running until Release.
        if let Phase::Thawed { .. } = cx.phase {
            cx.wait_for_command().await;
            continue;
        }

        // Safe point: between turns.
        if let Some(exit) = settle_drain(cx, DrainPoint::Turn).await? {
            return Ok(exit);                   // Quiesced or Closed
        }

        let Some(input) = cx.inbox.take_next_turn() else {
            cx.phase = Phase::Idle;
            cx.wait_for_command().await;       // a Send, Drain or Cancel wakes the agent
            continue;
        };

        cx.phase = Phase::Running { turn: TurnId::new() };
        let end = run_turn(cx, input).await?;
        dispatch(cx, Hook::TurnEnded(&end)).await?;                // [hook] turn_ended
    }
}
```

### A turn

```rust
async fn run_turn(cx: &mut AgentCtx, input: Input) -> Result<TurnEnd, DriverError> {
    cx.log.append(Event::TurnStarted { turn: cx.turn_id(), input })?;
    let limits = &cx.config.driver.limits;                           // [config]
    let mut forced = 0;

    let end = loop {
        // Safe point: between steps. A snapshot may be taken; a drain may stop here.
        maybe_snapshot(cx, SafePoint::StepBoundary).await?;
        cx.apply_pending_commands()?;
        if cx.drain_point_reached(DrainPoint::Step) {
            break TurnEnd::Suspended;          // turn stays open; resumes without repair
        }
        if cx.cancel.is_cancelled() { break TurnEnd::Cancelled; }
        if cx.counters.steps_this_turn >= limits.max_steps_per_turn { // [config] per prompt
            break TurnEnd::StepLimit;
        }

        let mut step = StepInput::from(cx.inbox.take_next_step());   // logged on enqueue
        match dispatch(cx, Hook::PreStep(&mut step)).await? {         // [hook] pre_step
            Flow::Reject(why) => break TurnEnd::Blocked(why),
            _ => {}
        }

        match run_step(cx, step).await? {
            StepOutcome::ToolsRan       => continue,     // the model is owed a reply
            StepOutcome::MaxTokens      => break TurnEnd::MaxTokens,
            StepOutcome::Failed(e)      => break TurnEnd::Error(e),
            StepOutcome::Cancelled      => break TurnEnd::Cancelled,
            StepOutcome::Concluded | StepOutcome::Answered => {}
        }

        // Continuation is data: another step happens only if something is in next_step.
        if cx.inbox.next_step.is_empty() {
            dispatch(cx, Hook::TurnStopping).await?;                 // [hook] turn_stopping
        }
        if cx.inbox.next_step.is_empty() { break TurnEnd::Completed; }
        forced += 1;
        if forced > limits.max_forced_continuations {                // [config]
            break TurnEnd::ContinuationLimit;
        }
    };

    if end != TurnEnd::Suspended {
        cx.log.append(Event::TurnEnded { end: end.clone() })?;
    }
    cx.log.sync().await?;                       // save point: turn boundary
    Ok(end)
}
```

### A step

```rust
async fn run_step(cx: &mut AgentCtx, step: StepInput) -> Result<StepOutcome, DriverError> {
    cx.log.append(Event::StepStarted { input: step.admitted })?;     // admitted once, never on retry

    let reply = loop {
        let mut route = cx.config.driver.default_route.clone();       // [config]
        dispatch(cx, Hook::Request(&mut route)).await?;               // [hook] request

        let request = build_request(&cx.log, &route, &cx.plugins.tool_specs());  // from the log only
        cx.log.sync().await?;                    // save point: before the model request

        let _slot = cx.quota.acquire_model_slot().await?;             // tenant fairness
        let model = cx.plugins.service::<dyn ModelProvider>(&route.provider)?;
        let attempt = stream_reply(cx, model, request).await;         // streams to observers, not logged

        match attempt {
            Ok(reply) => break reply,
            Err(err) => {
                cx.log.append(Event::AttemptFailed { error: err.summary() })?;
                let mut decision = ErrorDecision::Fail;
                dispatch(cx, Hook::RequestError(&err, &mut decision)).await?; // [hook] request_error
                match decision {
                    ErrorDecision::Retry => continue,
                    ErrorDecision::Fail  => return Ok(StepOutcome::Failed(err.into())),
                }
            }
        }
    };

    cx.log.append(Event::AssistantReply(reply.clone()))?;
    let mut verdict = ResponseVerdict::Proceed;
    dispatch(cx, Hook::Response(&reply, &mut verdict)).await?;       // [hook] response

    if reply.finish == Finish::MaxTokens { return Ok(StepOutcome::MaxTokens); }
    if verdict == ResponseVerdict::StopTurn { return Ok(StepOutcome::Concluded); }
    if reply.tool_calls.is_empty() { return Ok(StepOutcome::Answered); }

    run_tools(cx, reply.tool_calls).await
}
```

### Tools

Tool calls are prepared one at a time in model order, executed with overlap where the tool allows it, and committed in model order. This follows Grok Build for preparation and DeepSeek for commit order.

```rust
async fn run_tools(cx: &mut AgentCtx, calls: Vec<ToolCall>) -> Result<StepOutcome, DriverError> {
    // 1. Prepare sequentially: record, decide, ask.
    let mut prepared = Vec::with_capacity(calls.len());
    for mut call in calls {
        cx.log.append(Event::ToolCallRecorded { call: call.clone() })?;
        let mut decision = ToolDecision::Allow;
        dispatch(cx, Hook::ToolPre(&mut call, &mut decision)).await?;    // [hook] tool_pre
        if let ToolDecision::Ask(question) = decision {
            decision = ask_and_wait(cx, question).await?;   // may park the agent; see Quiesce
        }
        prepared.push((call, decision));
    }

    // 2. Execute: parallel-safe calls overlap; exclusive calls run alone, in order.
    let mut results = Vec::new();
    for group in group_by_concurrency(&prepared, &cx.plugins) {     // fail closed: unknown = exclusive
        for (call, _) in group.allowed() {
            cx.log.append(Event::ToolStarted { call_id: call.id })?;
        }
        cx.log.sync().await?;               // save point: "started" is durable before any body runs

        let futures = group.allowed().map(|(call, _)| {
            let tool = cx.plugins.tool(&call.name);
            let tcx = ToolCx::new(cx, call.id);       // read-only handles, grants, child cancel token
            wrap_execute(cx, tool, tcx, call.args)    // [hook] tool_execute wraps the future
        });
        let _slots = cx.quota.acquire_tool_slots(group.len()).await?;
        results.extend(join_all_bounded(futures, cx.config.driver.max_parallel_tools).await); // [config]
        results.extend(group.denied().map(ToolResult::denied));   // model-visible reason
    }

    // 3. Commit in model order.
    let mut concluded = false;
    for mut result in sort_by_model_order(results) {
        dispatch(cx, Hook::ToolPost(&mut result)).await?;            // [hook] tool_post
        cx.log.append(Event::ToolResult(result.clone()))?;
        for ctx_msg in result.deferred_context.drain(..) {
            cx.inbox.push_next_step(ctx_msg, &mut cx.log)?;          // logged on enqueue
        }
        concluded |= result.concludes_turn;
    }

    if cx.cancel.is_cancelled() { return Ok(StepOutcome::Cancelled); }
    Ok(if concluded { StepOutcome::Concluded } else { StepOutcome::ToolsRan })
}
```

### Stop conditions

A turn ends when the model answers without tool calls, a tool concludes the turn, the reply hits the output token limit, `pre_step` rejects, the user cancels, an error is not retried, or a limit is reached. Unlike DeepSeek, the driver has hard caps (`max_steps_per_turn`, `max_forced_continuations`) and they apply per prompt, not per round, which avoids the reset-per-round surprise noted in Grok Build. Each cap can be set high, but none is unlimited by default.

## Checkpoint and resume

### What is saved

The session log is the source of truth. It holds three kinds of record:

1. **A header** written once at creation: agent id, tenant, the full `AgentConfig` with every runner kind, code digest and grant, and each runner's `state_version`.
2. **Events**: message admissions (each with its envelope's timestamp and checksum), turn and step boundaries, admitted context, assistant replies, tool calls, starts and results, failed attempts, reconfigurations.
3. **Snapshots**: at a safe point, one record holding state slots (serialized with each runner's format and version), the inbox with its seen set, the counters, and the log offset it covers.

There are two kinds of snapshot, because crash recovery and quiesce have different needs.

- **Periodic snapshots** are optional and off by default (`[config] snapshot.every_steps`). Crash recovery replays the log from the start, or from the last periodic snapshot if there is one, and a little extra time on resume after a crash is acceptable. Plugins whose state can be derived from events (retry counters, repeat counters) mark their slot `derived` and rebuild it in `replay`, so a periodic snapshot may leave those slots out.
- **The quiesce snapshot** is complete. It holds every slot, derived ones included, plus the full inbox and seen set, so resuming from it needs no replay at all. The only new input on resume is whatever messages arrived while the agent was quiesced. Plugins with opaque state, such as a Python application, contribute a blob through the `snapshot` hook in both kinds.

### When it is forced to disk

| Save point | Why |
|---|---|
| Before each model request | The request can be rebuilt exactly; a crash during streaming loses only the partial reply |
| Before each tool body (`ToolStarted`) | Resume can tell "might have run" from "never ran" ([crash recovery note 2](../crash-recovery/README.md#notes-for-curio-harness)) |
| End of each turn | A finished turn is never lost |
| Message admission | A message the sender has been told was received is never lost |
| At quiesce | The agent can be resumed from the snapshot with no replay or repair |

Between save points the `SessionStore` plugin may batch writes. Periodic snapshots are an optimization for crash-resume time and never needed for correctness: resume from the header and the full event stream gives the same context. The quiesce snapshot is the exception, since a thawed agent is built from it directly.

Storage is configurable per agent: the `SessionStore` is a plugin, so the backend (local JSONL files, SQLite, an object store) is chosen in the agent's config. Moving an agent between hosts needs the new host to reach the same log, which means either a shared backend or a store plugin that ships the log with the quiesce snapshot.

### Resume

```rust
/// Take ownership of an agent on this host. Used after a crash, after a
/// local quiesce, and as the thaw step of a move between hosts.
pub async fn resume(rt: &Runtime, id: AgentId, start: StartMode)
    -> Result<AgentHandle, ResumeError>
{
    // 1. Exclusive ownership. Opening bumps the store's epoch; every later write
    //    carries it, and the store refuses writes from any older epoch.
    let store = rt.store_for(&id).open(&id, LeaseMode::Exclusive).await?;
    let header = store.read_header().await?;

    // 2. Rebuild plugins from the recorded config. A missing runner kind or a
    //    digest mismatch fails loud; nothing silently falls back.
    let mut cx = AgentCtx::build(rt, &header.config, store).await?;

    match cx.log.tail_marker().await? {
        // Fast path: the log ends with a quiesce marker and a complete snapshot.
        Some(Marker::Quiesced { snapshot, suspended_turn }) => {
            restore_slots(rt, &mut cx, &header, &snapshot, Completeness::Full)?;
            cx.inbox = snapshot.inbox;               // pending messages and seen set
            cx.counters = snapshot.counters;
            cx.log.append(Event::Thawed { host: rt.host_id(), epoch: cx.log.epoch() })?;
            if let Some(turn) = suspended_turn { cx.inbox.resume_turn(turn); }
        }

        // Slow path: no quiesce marker, so the process stopped without draining.
        _ => {
            let snap = cx.log.latest_snapshot().await?;          // periodic, if any
            if let Some(snap) = &snap {
                restore_slots(rt, &mut cx, &header, snap, Completeness::Partial)?;
                cx.inbox = snap.inbox.clone();
                cx.counters = snap.counters.clone();
            }
            // Replay events after the snapshot (or from the start). Hooks see
            // Replay and must not cause side effects.
            for event in cx.log.events_after(snap.map(|s| s.offset)).await? {
                cx.fold(&event);                                 // inbox, counters, phase
                dispatch(&mut cx, Hook::Replay(&event)).await?;  // [hook] replay
            }
            if let Tail::Crashed { dangling, .. } = classify_tail(&cx.log) {
                repair_crash(&mut cx, dangling)?;
                if header.config.resume.after_crash.should_continue(&cx.counters) { // [config]
                    cx.inbox.push_next_turn(Input::system(RESTARTED_NOTICE), &mut cx.log)?;
                }
            }
        }
    }

    // 3. Pull in anything this host spooled for the agent while it was not running here.
    for env in rt.spools.drain(&id).await? {
        cx.inbox.offer(env);                     // sorted insert; duplicates dropped
    }
    cx.log.sync().await?;

    // 4. Run now, or stay thawed (owned, accepting messages, not running) until Release.
    cx.phase = match start {
        StartMode::Run => Phase::Idle,
        StartMode::Thawed => Phase::Thawed { epoch: cx.log.epoch() },
    };
    Ok(rt.spawn_agent(cx))   // the Driver plugin's run() takes over
}

fn repair_crash(cx: &mut AgentCtx, dangling: Vec<DanglingCall>) -> Result<(), DriverError> {
    for call in dangling {
        let text = if call.started_durably {
            "Its outcome is unknown. Retry only if the operation is read-only or \
             idempotent; otherwise verify external state or ask the user first."
        } else {
            "It was not run. Retry it if it is still needed."
        };
        cx.log.append(Event::ToolResult(ToolResult::synthetic_error(call.id, text)))?;
    }
    cx.log.append(Event::StepEnded { reason: StepEnd::Interrupted })?;
    cx.log.append(Event::TurnEnded { end: TurnEnd::Interrupted })?;
    cx.counters.crash_resumes += 1;
    Ok(())
}
```

`restore_slots` decodes each slot with its plugin, calling `Runner::migrate` first when the slot's version is older than the runner's. With `Completeness::Full` a missing slot is an error, because the quiesce snapshot promised every slot; with `Partial`, derived slots are rebuilt by replay.

The crash branch uses DeepSeek's honest wording for an interrupted tool and opencode's optional automatic continuation with a resume cap. Whether a crashed turn continues by itself is the per-agent `resume.after_crash` setting: `wait` or `continue { max }`. It is a required field with no implicit default, so each deployment decides.

### Changing configuration

Configuration is pinned in the header, so a resumed agent behaves the way it did before. Changing it is a `Reconfigure` command. The driver applies it at the next safe point by appending a `Reconfigured { patch }` event and reloading the affected plugins; new plugin instances receive their state through `migrate` or start fresh, as their runner decides. Because the patch is in the log, a later resume replays it, and anything it changes in the model's input is reconstructible.

### Side effects outside the log

The log cannot undo a file edit or a sent message. Two hooks help: tool plugins receive the call id in `ToolCx` and are expected to use it as an idempotency key where the target supports one, and a workspace plugin can take a file snapshot in `pre_step` (opencode's approach) so the edits of an interrupted turn can be reverted. Background processes started by tools are recorded as events with their process ids, so a resumed agent can report or stop them instead of losing track of them.

## Quiesce and graceful shutdown

None of the three surveyed harnesses can finish the current step or turn and then stop cleanly; each one's shutdown cuts the turn off, and a resumed session looks like a crash. Here quiesce is a phase of the agent.

```rust
pub enum DrainPoint {
    Step,       // stop at the next step boundary; the turn is suspended and continues on resume
    Turn,       // let the current turn finish; stop before the next one
    Immediate,  // cancel the current step now; the log is closed like a cancel, then quiesced
}

pub enum DrainResult { Quiesced { at: LogOffset }, DeadlineHit { forced: bool }, Closed }
```

What happens on `Command::Drain { at, deadline }`:

1. The phase becomes `Draining`. New messages are still accepted, logged and acknowledged, so nothing a sender sends is lost, but no new turn starts.
2. `drain_requested` is dispatched. Plugins that own background work (jobs, subagents, schedulers) stop admitting new work and report what must finish first. A subagent is itself an agent, so the parent drains its children with the same command.
3. The driver runs until the requested safe point. A step that is waiting on a tool keeps waiting until the tool finishes or the deadline passes.
4. At the safe point the driver writes a complete snapshot, appends `Quiesced { at, suspended_turn }`, syncs, releases the lease, and returns `Exit::Quiesced`. The handle's waiters receive `DrainResult::Quiesced`. From this moment the host's ingress appends messages for the agent to its spool instead of a live inbox.
5. If the deadline passes first, the driver falls back to `Immediate`: it cancels running tool bodies through their child tokens, writes "not run" or "outcome unknown" results the same way crash repair would, and then quiesces. The result reports `DeadlineHit { forced: true }` so the operator knows the stop was not clean.

```rust
async fn settle_drain(cx: &mut AgentCtx, here: DrainPoint) -> Result<Option<Exit>, DriverError> {
    let Phase::Draining { at, deadline, .. } = &cx.phase else { return Ok(None) };
    if !(here.satisfies(*at) || Instant::now() >= *deadline) { return Ok(None); }

    let snapshot = write_snapshot(cx, Completeness::Full).await?;  // every slot, inbox, seen set
    cx.log.append(Event::Quiesced {
        snapshot: snapshot.offset,
        at: here,
        suspended_turn: cx.current_turn_if_suspended(),
        inbox_len: cx.inbox.len(),
    })?;
    cx.log.sync().await?;
    cx.log.release_lease().await?;
    cx.runtime().ingress.route_to_spool(cx.id);   // later messages go to this host's spool
    cx.phase = Phase::Quiesced;
    cx.notify_drain_waiters(DrainResult::Quiesced { at: cx.log.offset() });
    Ok(Some(Exit::Quiesced))
}
```

A process-level graceful shutdown is then a drain of every agent with one deadline: on SIGTERM the runtime sends `Drain { at: Turn, deadline }` to all handles, waits for the results, and exits. Resuming later is ordinary `resume`, which takes the fast path from the quiesce snapshot, merges the spool, and continues any suspended turn with no repair and no user message.

**Parking.** The same mechanism lets the runtime evict an agent that is waiting on something slow, such as a human approval in `tool_pre`. The agent writes a complete snapshot, appends `Parked { waiting_on }`, and its task exits; messages that arrive meanwhile go to the spool. An `Answer` command for that request makes the runtime resume it and deliver the answer. In a multi-tenant process this keeps memory proportional to agents that are doing work rather than agents that exist. If a drain reaches an agent that is parked, it stays parked: the pending request is neither denied nor answered, and it is still waiting when the agent is resumed, on this host or another.

## Inbound messages

Every message to an agent, from a user, another agent, a scheduler or a webhook, arrives in an envelope:

```rust
pub struct Envelope {
    pub agent: AgentId,
    pub sender: SenderId,
    pub sent_at: Timestamp,      // assigned once, by the sender or the cluster gateway
    pub delivery: Delivery,      // NextTurn | NextStep
    pub body: MessageBody,
    pub checksum: Digest,        // over a canonical encoding of all fields above
}

impl Envelope {
    fn key(&self) -> (Timestamp, Digest) { (self.sent_at, self.checksum) }
}
```

Two rules make deduplication work across hosts. The timestamp is assigned before the message is routed, never by the receiving host, so two copies of one message that reach different hosts are identical and have the same checksum. And the checksum covers the sender, the timestamp and the body, so two different messages with the same timestamp still sort deterministically and are not mistaken for each other. A sender that means to send the same text twice at the same instant must make the envelopes differ, for example with a sequence number in the body.

The inbox keeps two structures:

```rust
pub struct Inbox {
    pending: BTreeMap<(Timestamp, Digest), Envelope>,  // received, not yet taken into a turn or step
    seen: SeenSet,                                     // checksums of every envelope received, within the horizon
    next_step: Vec<Input>,                             // claimed for the current turn's next step
    newest: Timestamp,                                 // newest sent_at ever received
}

impl Inbox {
    /// Called for every envelope, from a live Send, a spool on this host, or a Merge from another host.
    fn offer(&mut self, env: Envelope, log: &mut SessionLog, cfg: &InboxConfig) -> Offer {
        if self.seen.contains(&env.checksum) { return Offer::Duplicate; }  // already received: ack, drop
        if env.sent_at < self.newest - cfg.dedupe_horizon {               // [config]
            // Too old to prove it is not a duplicate whose record was pruned.
            return match cfg.late_policy { LatePolicy::Refuse => Offer::Refused, LatePolicy::Admit => self.accept(env, log) };
        }
        self.accept(env, log)
    }

    fn accept(&mut self, env: Envelope, log: &mut SessionLog) -> Offer {
        log.append(Event::MessageReceived(env.clone()));   // durable before the sender is acknowledged
        self.seen.insert(env.checksum, env.sent_at);
        self.newest = self.newest.max(env.sent_at);
        self.pending.insert(env.key(), env);
        Offer::Accepted
    }

    /// The driver takes the earliest pending message for a new turn.
    fn take_next_turn(&mut self) -> Option<Input> { /* pop first NextTurn entry in key order */ }
}
```

Ordering is defined over what has not yet been consumed. Pending messages are always taken in `(timestamp, checksum)` order, so it does not matter which host received a message first or in what order forwarded batches arrive. Once a message has been taken into a turn, its place in the conversation is fixed and logged. A message that arrives after a later-stamped message was already taken is still admitted, at the front of the pending set, and its log record is marked `late`. With the migration sequence below, this happens only to messages that the old host received after the new host was released, which a short forwarding window keeps rare.

The seen set is pruned by timestamp once entries fall outside `dedupe_horizon`, and it is part of every snapshot, so deduplication survives quiesce, crash and migration. Senders are expected to retry until acknowledged, and a host acknowledges only after the envelope is durable in the agent's log or in a spool. Retries plus deduplication give each message exactly one admission.

## Migrating an agent between hosts

Resizing a cluster moves agents by quiescing them on one host and resuming them on another. The orchestrator drives the sequence; the agent's own behavior is unchanged, and nothing the model sees records the move. The sequence below follows the eight steps in the requirements.

| Step | Host A | Host B | Session store |
|---|---|---|---|
| 1. Running on A | Agent runs; messages go straight to its inbox | | Epoch `n`, owned by A |
| 2. Quiesce requested | `Drain { at, deadline }`; messages still logged into the inbox | | |
| 3. Quiescing and quiesced | Reaches the safe point, writes the complete snapshot and `Quiesced`, releases the lease; later messages go to A's spool | | Unowned; ends with `Quiesced` |
| 4. Thawed on B | | `resume(id, StartMode::Thawed)`: opens the store, restores the snapshot, appends `Thawed`; phase `Thawed` | Epoch `n+1`, owned by B |
| 5. Messages arrive at both | Spooled | Offered to the inbox: logged, sorted, deduplicated; no turn runs | |
| 6. A's messages added to B | Forwards its spool to B as `Merge` batches; deletes each batch only after B acknowledges it | Offers each envelope; duplicates are dropped | |
| 7. B restarts the agent | Keeps forwarding anything new | `Release`: phase becomes `Idle`; the suspended turn continues, then pending messages run in key order | |
| 8. A torn down | Stops acknowledging (senders retry against B), forwards its final batch, waits for the acknowledgment, deletes the spool | | |

```rust
/// Orchestrator side. Each call is idempotent so the whole sequence can be retried.
async fn migrate(id: AgentId, a: &HostClient, b: &HostClient, cfg: &MigrationConfig) -> Result<(), MigrateError> {
    a.drain(id, cfg.drain_point, cfg.drain_deadline).await?;          // steps 2-3
    b.thaw(id).await?;                                                // step 4
    cfg.directory.point(id, b.host()).await?;                         // step 5 begins as routes update
    a.start_forwarding(id, b.host()).await?;                          // step 6, continues until teardown
    a.wait_spool_drained_once(id).await?;                             // everything A held at this point is on B
    b.release(id).await?;                                             // step 7
    a.teardown_after(cfg.forward_grace).await?;                       // step 8
    Ok(())
}
```

Three properties make the move transparent.

1. **Nothing is lost.** Every message is durable before it is acknowledged, in A's log, A's spool or B's log, and A deletes spooled messages only after B has made them durable.
2. **Nothing is admitted twice.** The seen set travels in the quiesce snapshot, so B recognizes messages A had already received, and any message delivered to both hosts or forwarded twice has one checksum.
3. **Only one host writes.** Opening the store for B raises the epoch, and the store refuses any append that carries an older one. If A were still running because of a bug or a partition, its writes would fail rather than fork the log. The same fencing covers a failed move: if B dies while thawed, the orchestrator thaws the agent on another host, which raises the epoch again, and A's forwarding target changes with the directory.

Waiting for A's spool to drain once before releasing B means everything that arrived during the quiesce window is sorted before B runs anything. Messages that reach A after that are still delivered, as late arrivals, until A is torn down.

## Isolation in a multi-tenant process

Isolation has four layers, from the language up to the operating system.

**State.** All mutable per-agent state is in `AgentCtx`, owned by one task. Plugin instances are immutable and runners hold only tenant-neutral resources. The rules that keep it that way are enforced in review and, where possible, by lints: no `static` with interior mutability in runner crates, no `thread_local!` or `task_local!` carrying agent data, no `Mutex` inside a plugin instance. Because instances are immutable, two agents with identical plugin config could share one instance safely (the equivalent of DeepSeek's shared preset generations); that is an optimization to adopt only after measuring.

**Capabilities.** A plugin can reach nothing it was not granted. `PluginGrants` come from the agent config and name the services it may call, the hooks it may subscribe to, filesystem roots, network hosts, and secrets. Secrets are references (`secret://tenant/name`) resolved through the runtime per call, never stored in the log or in state slots. Tools receive a `ToolCx` with only that call's grants. Shared resources such as an HTTP connection pool are safe only if credentials travel with each request, which is the rule for every runner.

**Faults and resources.** Each agent task runs under `catch_unwind` at its top level; a panic in a plugin fails that agent, writes a crash marker if possible, and leaves the process and other agents running. Model calls and tool bodies acquire slots from the tenant's `QuotaHandle`, so one tenant cannot starve the rest. WASM plugins run with per-instance fuel and memory limits. A quota breach pauses or fails the agent; it never aborts the process.

**Code.** The runner kind decides how far untrusted code is from the host:

- `builtin` code is trusted Rust, configured by data. It is as isolated as the context and grants make it.
- `wasm` code runs in the host but in its own linear memory, one instance per plugin per agent. This is the preferred way to run tenant-supplied code in-process.
- `process` code runs in a separate OS process with the harness sandbox applied. Embedding CPython or a JS engine directly in the host is ruled out for tenant code: interpreter state and native extensions are shared across everything in the process, which breaks the isolation requirement.

## Worked example

Two agents in the same process, owned by different tenants and configured differently. Each is one TOML document; the runtime records it in the agent's log header at creation.

```toml
# Agent A: a coding agent with only built-in plugins.
[agent]
id = "acme-coder-17"
tenant = "acme"

[[plugin]]
id = "driver"
runner = "builtin.react-driver"
code.inline = { max_steps_per_turn = 200, max_forced_continuations = 8, max_parallel_tools = 8 }

[[plugin]]
id = "model"
runner = "builtin.model.openai-compat"
code.inline = { base_url = "https://api.example.com/v1", model = "coder-large" }
grants = { secrets = ["secret://acme/model-key"], hosts = ["api.example.com"] }

[[plugin]]
id = "store"
runner = "builtin.store.jsonl"
code.inline = { snapshot_every_steps = 20 }

[[plugin]]
id = "compaction"
runner = "builtin.compaction"
code.inline = { threshold_ratio = 0.8 }

[[plugin]]
id = "bash"
runner = "builtin.tool.bash"
code.inline = { sandbox = "workspace-write" }
grants = { fs = ["/work/acme-coder-17"] }

[resume]
after_crash = { continue = { max = 3 } }   # unattended, so a crashed turn continues
```

```toml
# Agent B: a support agent whose triage policy is a Python application.
[agent]
id = "globex-support-4"
tenant = "globex"

[[plugin]]
id = "driver"
runner = "builtin.react-driver"
code.inline = { max_steps_per_turn = 30, max_forced_continuations = 2, max_parallel_tools = 2 }

[[plugin]]
id = "model"
runner = "builtin.model.openai-compat"
code.inline = { base_url = "https://llm.globex.internal/v1", model = "support-small" }
grants = { secrets = ["secret://globex/model-key"], hosts = ["llm.globex.internal"] }

[[plugin]]
id = "store"
runner = "builtin.store.sqlite"
code.inline = { path = "/data/globex/sessions.db" }

[[plugin]]
id = "triage"
runner = "process.python"
code.package = { name = "globex-triage", version = "2.3.1", digest = "sha256:9f2c..." }
grants = { hooks = ["pre_step", "tool_pre", "turn_stopping", "snapshot"], hosts = ["crm.globex.internal"] }

[[plugin]]
id = "crm"
runner = "mcp"
code.inline = { command = ["globex-crm-mcp"], tools = ["lookup_customer", "open_ticket"] }

[resume]
after_crash = "wait"

[inbox]
dedupe_horizon = "24h"
late_policy = "refuse"
```

What the loop does with B's Python plugin:

- At creation, the `process.python` runner fetches the package by digest, starts a child process for this agent, and asks it which hooks it handles. It may subscribe only to the granted hooks; anything else fails creation.
- On `pre_step`, the runner sends the step input as JSON. The Python code returns context to add (for example, the customer's open tickets); the runner appends it as an event, so the model sees it and a resume reproduces it.
- On `tool_pre` for `open_ticket`, the Python code returns `ask`, so the driver asks a human and the agent parks until the answer arrives.
- On `snapshot`, the Python code returns its own state as JSON, which goes into B's snapshot record. On resume, a new child process receives it before any events are replayed.
- If the Python process dies, the runner reports a `PluginError` on the next hook, which fails B's current turn; A is unaffected.

## Decisions and open questions

Decided after review of the first draft:

1. **Behavior after a crash is configuration.** `resume.after_crash` is a required per-agent field, `wait` or `continue { max }`, with no implicit default.
2. **Crash resume replays the log; quiesce resume does not.** Periodic snapshots are optional and off by default, since a slower resume after a crash is acceptable. The quiesce snapshot is complete, so a quiesced agent is rebuilt from it plus the messages that arrived while it was quiesced.
3. **One hook definition for every non-Rust runner.** The hooks are defined once as WIT, and the JSON-RPC schema for `process` runners and the Rust bindings are generated from it.
4. **`process` plugins run one process per agent.**
5. **A drain does not deny pending approvals.** A parked agent stays parked through a quiesce or a move, and the request is still waiting when it resumes.
6. **Storage backends are configurable** through the `SessionStore` plugin, per agent.

Still open:

1. **Late arrivals.** The default for `late_policy` (refuse or admit) and the length of `dedupe_horizon`. Refusing is safer against duplicates; admitting is safer against loss when a host was partitioned for longer than the horizon.
2. **Where timestamps come from.** Sender clocks are simplest, but skew between senders changes the order in which their messages are taken. A cluster gateway that stamps every message gives one clock, at the cost of a component every message must pass through.
3. **Telling the model about late messages.** A late message is logged with a `late` flag. Whether the request builder should show that to the model, for example as "this message was sent before your last reply", is a prompt-design question.

## What this borrows

| Idea | Source |
|---|---|
| The loop is itself a replaceable plugin; policy lives on a short list of typed hooks | DeepSeek Harness |
| Continuation is data: another step happens only if `next_step` is non-empty | DeepSeek Harness |
| Requests rebuilt from the log; model-visible input is always logged | DeepSeek Harness, opencode |
| Retries never re-admit user input, and recovery plugins prove progress before asking for a retry | DeepSeek Harness |
| `ToolStarted` forced to disk before the body runs; honest "outcome unknown" repair | DeepSeek Harness |
| Optional automatic continuation after a crash, with a resume cap | opencode |
| Sequential preparation of tool calls, parallel execution, typed stop outcomes | Grok Build |
| Hard caps on steps and forced continuations | Grok Build (forced continuations), opencode (steps) |
| Session actor that owns its state and takes commands over a channel | Grok Build |
| Runner and code split, immutable plugin instances, state slots, drain, park and thaw phases | New in this proposal |
| Sender-stamped envelopes sorted and deduplicated by `(timestamp, checksum)`, host spools, epoch fencing for moves between hosts | New in this proposal |
