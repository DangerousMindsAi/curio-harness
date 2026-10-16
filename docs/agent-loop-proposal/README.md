# Agent loop proposal

*A plugin-first agent loop for curio-harness, with checkpoint, resume and graceful quiesce, for many isolated agents in one process.*

Status: accepted. A first implementation is in `crates/`; the [README](../../README.md#status-against-the-proposal) lists where it differs.

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
- [Message queue](#message-queue)
- [Migrating an agent between hosts](#migrating-an-agent-between-hosts)
- [Creating an agent](#creating-an-agent)
- [Isolation in a multi-tenant process](#isolation-in-a-multi-tenant-process)
- [Worked example](#worked-example)
- [Decisions and open questions](#decisions-and-open-questions)
- [What this borrows](#what-this-borrows)

## Summary

The process hosts a `Runtime` that holds only shared, immutable machinery: a registry of runners, a scheduler, and two runtime-level plugins, the log store and the provisioner. Each agent is one async task that exclusively owns an `AgentCtx`, and every piece of per-agent state lives in that struct. Nothing per-agent is reachable through a global, a static, or a task-local.

Every behavior is a plugin, including the loop driver, the model provider, the message queue and each tool. A plugin is a pair: a **runner**, which is Rust code compiled into the host and implementing the `Runner` trait, and **code**, which is whatever the runner interprets. For a built-in runner the code is a block of configuration; for a WASM or Python runner it is a whole program. Runners are shared by every agent in the process and hold no agent state. Loading code through a runner produces a plugin instance that is immutable after load; its mutable state lives in a typed slot in the agent's context. This split is what makes checkpointing uniform and isolation checkable: the harness can serialize every slot without trusting each plugin to remember to, and a plugin has nowhere else to keep state.

The default driver is a small loop in the style of DeepSeek Harness. Each turn is a series of steps; each step is one model request (with retries) plus the tools that request asks for. Plugins intercept it at a short list of typed hooks. Every input the model sees is first appended to the agent's session log, and the conversation each request is built from is a projection of that log, so the log is enough to recover after a crash.

Crash recovery is event-sourced. The log is forced to durable storage before every model request and before every tool body. After a crash, the context is rebuilt from the configuration recorded in the log, the events are replayed, and loose ends are patched the same way DeepSeek does.

Quiesce is a first-class phase. A drain request stops new work from being admitted, lets the agent reach a chosen safe point (end of step or end of turn), and serializes the whole context into an `AgentImage`. Thawing that image, on the same host or another, gives back the agent exactly where it stopped, with no log replay, no repair and no user message needed. None of the three surveyed harnesses can do this.

Inbound messages wait in a shared message queue, which is itself a plugin. An agent pulls from its queue only when it is ready to use a message, so a quiesced agent simply stops pulling and anything sent meanwhile stays in the queue. That makes moving an agent between hosts, when a cluster is resized, three steps: quiesce on the old host, transfer the image (outside the harness), and thaw on the new host. A fencing epoch on the log guarantees that only one host can append to it at a time.

A new agent's context comes from a provisioner plugin. The provisioner is the one plugin configured outside any agent, in the process's own startup file; given a request to create an agent, it returns the agent's configuration and initial state. From then on the agent is rebuilt from its own log and never asks the provisioner again.

## Requirements and where each is met

| Requirement | Where it is met |
|---|---|
| Everything is a plugin | The driver, model, store, tools and all policy are plugins behind service kinds and hooks ([Runners and code](#runners-and-code), [Hooks](#hooks)). Swapping the driver swaps the loop. |
| Written in Rust | Runner and plugin interfaces are Rust traits; the agent is a single-owner task, so the context needs no locks ([The agent context](#the-agent-context)). |
| Checkpoint and resume | A serializable context that quiesce turns into an image and thaw restores; write-ahead save points and log replay for crashes ([Checkpoint and resume](#checkpoint-and-resume), [Quiesce](#quiesce-and-graceful-shutdown)). |
| Many agents per process, configured independently, strictly isolated, all state in a context variable | One `AgentCtx` per agent holding all mutable state; immutable plugin instances; per-agent capability grants and quotas; isolation level chosen per runner ([Isolation](#isolation-in-a-multi-tenant-process)). |
| Plugins are a runner plus code | `Runner::load(code) -> Plugin` with code being configuration, a WASM component, or an external program ([Runners and code](#runners-and-code)). |
| Move agents between hosts transparently when resizing a cluster | Messages wait in a shared queue plugin; quiesce produces an image on the old host and thaw restores it on the new one ([Message queue](#message-queue), [Migrating an agent between hosts](#migrating-an-agent-between-hosts)). |
| Initial configuration is also a plugin | A provisioner plugin, named in the process's startup file, produces each new agent's configuration and initial state ([Creating an agent](#creating-an-agent)). |

## Vocabulary

- **Runtime**: the per-process host. Holds the runner registry, the scheduler, the log store and the provisioner. Shared by all agents, never mutated by them.
- **Agent**: one configured conversation partner with its own session log. Owned by exactly one task.
- **AgentCtx**: the struct that holds all of one agent's mutable state. The "context variable" from the requirements.
- **Runner**: a Rust type implementing `Runner`, registered once per process under a runner name such as `builtin.compaction` or `wasm`. An agent's config picks the runner for each plugin by that name.
- **Code**: what a runner loads for one plugin: an inline config table, a file, or a versioned package, always pinned by digest.
- **Plugin instance**: what `Runner::load` returns for one agent. Immutable after load.
- **State slot**: the mutable, serializable state of one plugin instance, stored in `AgentCtx`.
- **Service**: a capability with exactly one provider per agent (`Driver`, `Model`, `MessageQueue`, `Approval`) or many (`Tool(name)`).
- **Hook**: a typed interception point the driver or tool pipeline dispatches to every subscribed plugin, in order.
- **Turn / step / attempt**: as in DeepSeek. A turn answers one input; a step is one model request plus its tools; an attempt is one try at the request.
- **Safe point**: a moment where the agent's state is consistent and can be snapshotted or quiesced: before a step starts, or after a turn ends.
- **Message queue**: the service an agent's inbound messages wait in until the agent pulls them. Provided by a plugin, so the backend (a shared broker, a database table, an in-process channel for tests) is configuration.
- **Log store**: where session logs are kept. Part of the hosting infrastructure: a runtime-level plugin configured once per process, the same for every tenant, and not necessarily local.
- **Provisioner**: the plugin that produces a new agent's configuration and initial state. Like the log store, configured per process instead of per agent.
- **Agent image**: the serialized `AgentCtx` of a quiesced agent. Quiesce produces it and thaw consumes it; how it travels between hosts is outside the harness.
- **Epoch**: a counter on an agent's log that increases each time a host takes ownership. Appends carrying an older epoch are refused.

## Process layout

```text
process
└── Runtime (shared, immutable after start)
    ├── RunnerRegistry: runner name -> Arc<dyn Runner>
    ├── LogStore:       runtime-level plugin; where every agent's session log lives (any backend)
    ├── Scheduler:      per-tenant quotas, admission, fair sharing of model/tool slots
    ├── Provisioner:    runtime-level plugin; creates new agents' configs and initial state
    └── agents: AgentId -> AgentHandle      (a channel sender, no state)

agent task (one per live agent, owns everything below)
└── AgentCtx
    ├── config          pinned in the log at creation
    ├── plugins         immutable instances, in hook order, plus a service index
    ├── state           one slot per plugin
    ├── log             append-only session log (via the LogStore plugin)
    ├── history         the conversation the next request is built from; a projection of the log
    ├── inbox           messages pulled from the queue and not yet used; next_turn / next_step
    ├── queue           this agent's consumer on its message queue (via the MessageQueue plugin)
    ├── phase           Idle | Running | Draining | Parked | Quiesced | Closed
    ├── counters        steps, turns, forced continuations, tokens
    ├── grants          what this agent may touch (fs roots, hosts, secrets, services)
    └── cancel, quota   cancellation token, tenant quota handle
```

Outside callers talk to an agent only through its `AgentHandle`:

```rust
pub enum Command {
    Cancel { reason: CancelReason },                 // abort the current turn
    Drain { at: DrainPoint, deadline: Instant, reply: oneshot::Sender<DrainResult> },
    Answer { request: RequestId, answer: Answer },   // approval or question from a plugin
    Reconfigure { patch: ConfigPatch },              // applied at the next safe point
}

pub struct AgentHandle { tx: mpsc::Sender<Command>, id: AgentId, tenant: TenantId }
```

The agent task is an actor. Commands are drained at defined points in the loop, so nothing outside the task ever holds a reference into `AgentCtx`. Messages for the model do not travel as commands; they go through the agent's message queue.

## The agent context

```rust
/// All mutable state for one agent. Owned by the agent's task; never shared.
pub struct AgentCtx {
    pub id: AgentId,
    pub tenant: TenantId,
    config: Arc<AgentConfig>,        // as recorded in the log; changes only via Reconfigure
    plugins: PluginTable,            // Box<dyn Plugin> in hook order + service index
    state: StateSlots,               // PluginId -> Box<dyn PluginState>
    log: SessionLog,                 // append and sync; backed by the runtime's LogStore
    history: History,                // model-visible conversation; changes only through log.append
    inbox: Inbox,                    // pulled, logged and acknowledged, not yet used by a turn or step
    queue: QueueConsumer,            // exclusive consumer of this agent's queue
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
    Quiesced,                          // image produced, lease released, queue detached; task has exited
    Closed,
}

/// Mutable state of one plugin. Serialized into images and snapshots; restored on thaw or recovery.
pub trait PluginState: Send + erased_serde::Serialize + Any {}
```

Three properties follow from this layout.

1. **The context is a parameter, never ambient.** Every hook receives a borrow of the context (or a narrower view of it), so a plugin that tries to reach another agent's state has nothing to name. A `tokio::task_local!` would look tidier, but it silently disappears in tasks spawned for tool bodies and makes cross-agent leaks a runtime question instead of a compile-time one.
2. **One owner, no locks.** The agent task holds `&mut AgentCtx` for its whole life. Concurrency inside an agent (parallel tool bodies) gets a read-only `ToolCx` built from the context, and any change those bodies want to make comes back as data in their results.
3. **The context is serializable.** Everything in it is either data (`config`, `state`, `inbox`, `history`, `counters`) or rebuilt from that data on load (the plugin table, grants, quota and the queue consumer). Serializing it is how an agent is quiesced and moved; it can also be rebuilt from the log alone, which is how it recovers from a crash.

## Runners and code

```rust
/// Compiled into the host. Registered once per process under a runner name, e.g.
/// `registry.register("wasm", WasmRunner::new())`; shared by every agent.
/// Holds only immutable, tenant-neutral resources (a WASM engine, a parser).
pub trait Runner: Send + Sync + 'static {
    /// Version of the state-slot format this runner writes. Bumped on incompatible change.
    fn state_version(&self) -> u32;

    /// Validate and load `code` for one agent. Misconfiguration fails here, at creation,
    /// thaw or recovery, never later in the loop.
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

/// Runtime-level: one instance per process, configured in the startup file.
pub trait LogStore: Plugin {
    /// Open an agent's log for appending. Raises the log's epoch; appends from older epochs fail.
    async fn open(&self, id: &AgentId, mode: OpenMode) -> Result<SessionLog, StoreError>;
}

pub trait MessageQueue: Plugin {
    /// Become the only consumer of this agent's queue; see Message queue.
    async fn attach(&self, agent: &AgentId) -> Result<QueueConsumer, QueueError>;
}
```

The `Provisioner` trait is described in [Creating an agent](#creating-an-agent). `LogStore` and `Provisioner` are the two runtime-level services: their plugins are named in the process's startup file instead of in an agent's config, because they are part of the hosting infrastructure and are needed before any agent exists.

### Runners

The runner decides what "code" means and how strongly that code is isolated. Proposed initial set:

| Runner name | Code | Where the code runs | Isolation |
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
| `agent_loaded` | After create, thaw or recovery, before any work | May enqueue context into `next_step` | Instructions loader, session-start hooks |
| `pre_step` | At the start of each step, after claiming `next_step` input | Add context messages, compact history, `Reject` (turn ends blocked) | Compaction, time context, skills, plan mode, goal driver |
| `request` | Before each attempt | Model route, effort, max tokens, tool choice | Model selection, cost router |
| `request_error` | After a failed attempt | Set `decision = Retry` only after proving progress (for example, compaction changed the history) | Retry with backoff, overflow compaction, image offload |
| `response` | After the reply is logged, before tools run | Mark the step as stalled or stop the turn | Stationarity detector, refusal handling |
| `tool_pre` | Per call, sequentially in model order | Allow, deny with a model-visible reason, ask a human, rewrite arguments (logged) | Permission policy, hooks bridges, auto-review |
| `tool_execute` | Wraps each tool body; may run concurrently | Wrap the future (timeout, sandbox); read-only `ToolCx` | Timeout, sandbox, metering |
| `tool_post` | Per result, sequentially in model order | Rewrite the result, attach context for the next step, mark the turn concluded | Spill, repeat reminders |
| `turn_stopping` | When nothing is owed and `next_step` is empty | Enqueue into `next_step` to force another step | Stop hooks, todo gate, goal verification |
| `turn_ended` | After the turn's end record is written | Enqueue into `next_turn` | Goal round driver, notifications |
| `snapshot` | When an image or a periodic snapshot is taken | Contribute opaque state (only plugins whose state is not in a slot, such as `process` plugins) | Python and WASM runners |
| `replay` | For each event replayed during crash recovery | Rebuild derived state in the slot; no side effects allowed | Retry counters, repeat counters |
| `drain_requested` | When a drain is requested | Stop admitting background work; report what must finish | Background jobs, subagents, schedulers |

Two rules hold for every hook:

1. **Model-visible implies logged.** A hook that changes what the model will see does so by appending an event through `HookCx::append`. The request builder reads only `history`, which changes only when an event is appended, so a hook cannot inject anything that a recovered agent would not also see.
2. **Hooks that run concurrently get no mutable state.** `tool_execute` is the only hook that can run in parallel with itself, so it receives `&ToolCx`, not `&mut HookCx`. Anything it learns returns inside the `ToolResult` and is folded into state by `tool_post`, which runs in model order.

## The loop

This is the default `Driver` plugin, `builtin.react-driver`. Lines marked `// [hook]` dispatch to plugins and lines marked `// [config]` read the driver's code table.

### Driving the agent

```rust
async fn run(&self, cx: &mut AgentCtx) -> Result<Exit, DriverError> {
    dispatch(cx, Hook::AgentLoaded).await?;                       // [hook] agent_loaded
    loop {
        cx.apply_pending_commands()?;          // Cancel, Drain, Answer, Reconfigure

        // Safe point: between turns.
        if let Some(exit) = settle_drain(cx, DrainPoint::Turn).await? {
            return Ok(exit);                   // Quiesced or Closed
        }

        if cx.inbox.is_empty() {
            pull_messages(cx, Pull::Wait).await?;  // idle: wait for a message or a command
        }
        let Some(input) = cx.inbox.take_next_turn() else {
            cx.phase = Phase::Idle;
            continue;                          // woken by a command, not a message
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
        // Safe point: between steps. A periodic snapshot may be taken; a drain may stop here.
        maybe_snapshot(cx, SafePoint::StepBoundary).await?;
        cx.apply_pending_commands()?;
        if cx.drain_point_reached(DrainPoint::Step) {
            break TurnEnd::Suspended;          // turn stays open; resumes without repair
        }
        if cx.cancel.is_cancelled() { break TurnEnd::Cancelled; }
        if cx.counters.steps_this_turn >= limits.max_steps_per_turn { // [config] per prompt
            break TurnEnd::StepLimit;
        }

        pull_messages(cx, Pull::NoWait).await?;                       // steering sent during the last step
        let mut step = StepInput::from(cx.inbox.take_next_step());   // logged on pull
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

        let request = build_request(&cx.history, &route, &cx.plugins.tool_specs());  // log projection only
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

An agent is restored in one of two ways, depending on whether it stopped cleanly.

| | Quiesce, then thaw | Crash, then recover |
|---|---|---|
| What it starts from | The `AgentImage` produced by the drain | The session log |
| Reads the log? | No; it only appends to it | Yes; replays it |
| Repair needed? | No | Yes: dangling tool calls, open step and turn |
| Cost | Deserialize one image | Replay the log (a little slower, which is acceptable) |

### The agent image

```rust
/// A quiesced agent: the whole AgentCtx as data. Produced by settle_drain;
/// moving it between hosts is outside the harness.
#[derive(Serialize, Deserialize)]
pub struct AgentImage {
    pub id: AgentId,
    pub tenant: TenantId,
    pub config: AgentConfig,                       // every plugin: runner name, code digest, grants
    pub slots: Vec<(PluginId, u32, StateBlob)>,    // every slot, with its runner's state_version
    pub history: History,                          // what the next model request is built from
    pub inbox: Inbox,                              // pulled and logged, not yet used
    pub counters: Counters,
    pub suspended_turn: Option<TurnId>,            // set when drained at a step boundary
    pub log_position: LogPosition,                 // offset and epoch the image is consistent with
}
```

Plugins with state outside a slot, such as a Python application in its own process, contribute a blob through the `snapshot` hook, and it goes into `slots` like any other. The image does not contain the plugin instances, grants, quota handle or queue consumer: those are rebuilt on thaw from `config` by the same path that creates an agent.

### The session log

The log is the durable record used for crash recovery and for audit. It lives in the runtime's `LogStore`, which is hosting infrastructure: configured once per process, the same for every tenant, and not necessarily on the local machine. Any host can therefore open any agent's log by its id. It holds:

1. **A header** written once at creation from the provisioner's output: agent id, tenant, the full `AgentConfig` with every runner name, code digest and grant, and each runner's `state_version`.
2. **Events**: messages pulled from the queue (with their queue ids and send times), turn and step boundaries, admitted context, assistant replies, tool calls, starts and results, failed attempts, reconfigurations, and `Quiesced` and `Thawed` markers.
3. **Periodic snapshots**, optional and off by default (`[config] snapshot.every_steps`), which only shorten crash recovery. Plugins whose state can be derived from events mark their slot `derived` and rebuild it in `replay`.

### When it is forced to durable storage

| Save point | Why |
|---|---|
| Before each model request | The request can be rebuilt exactly; a crash during streaming loses only the partial reply |
| Before each tool body (`ToolStarted`) | Recovery can tell "might have run" from "never ran" ([crash recovery note 2](../crash-recovery/README.md#notes-for-curio-harness)) |
| End of each turn | A finished turn is never lost |
| After pulling messages, before acknowledging them to the queue | A message leaves the queue only once it is in the agent's log |
| At quiesce, before the image is handed over | The log and the image agree on `log_position` |

Between save points the log store plugin may batch writes.

### Thaw

```rust
/// Restore a quiesced agent from its image, on this host or any other.
pub async fn thaw(rt: &Runtime, image: AgentImage) -> Result<AgentHandle, ThawError> {
    // 1. Take ownership of the log. Raises the epoch, so a stale host can no longer append.
    //    Refuses if the log has moved past the image: the image is out of date.
    let log = rt.log_store.open(&image.id, OpenMode::ContinueFrom(image.log_position)).await?;

    // 2. Rebuild plugins from the config in the image. A missing runner name or a
    //    digest mismatch fails loud; nothing silently falls back.
    let mut cx = AgentCtx::build(rt, &image.config, log).await?;

    // 3. Restore every slot, migrating any whose runner version moved.
    for (pid, version, blob) in image.slots {
        cx.state.insert(&pid, restore_slot(rt, &cx, &pid, version, blob)?);
    }
    cx.history = image.history;
    cx.inbox = image.inbox;
    cx.counters = image.counters;
    if let Some(turn) = image.suspended_turn { cx.inbox.resume_turn(turn); }

    cx.log.append(Event::Thawed { host: rt.host_id(), epoch: cx.log.epoch() })?;
    cx.queue = cx.plugins.service::<dyn MessageQueue>()?.attach(&cx.id).await?;
    cx.phase = Phase::Idle;
    Ok(rt.spawn_agent(cx))   // the Driver plugin's run() takes over
}
```

`restore_slot` decodes the blob with its plugin, calling `Runner::migrate` first when the slot's version is older than the runner's. An image is missing no slots by construction, so thaw never replays events.

### Crash recovery

```rust
/// Rebuild an agent that stopped without quiescing, from its log alone.
pub async fn recover(rt: &Runtime, id: AgentId) -> Result<AgentHandle, RecoverError> {
    let log = rt.log_store.open(&id, OpenMode::Recover).await?;   // raises the epoch
    let header = log.read_header().await?;
    let mut cx = AgentCtx::build(rt, &header.config, log).await?;

    let snap = cx.log.latest_snapshot().await?;                   // periodic, if any
    if let Some(snap) = &snap { cx.restore_partial(rt, snap)?; }
    // Replay events after the snapshot (or from the start). Hooks see Replay and
    // must not cause side effects.
    for event in cx.log.events_after(snap.map(|s| s.offset)).await? {
        cx.fold(&event);                                  // history, inbox, counters, phase
        dispatch(&mut cx, Hook::Replay(&event)).await?;   // [hook] replay
    }

    if let Tail::Crashed { dangling, .. } = classify_tail(&cx.log) {
        repair_crash(&mut cx, dangling)?;
        if header.config.resume.after_crash.should_continue(&cx.counters) { // [config]
            cx.inbox.push_next_turn(Input::system(RESTARTED_NOTICE), &mut cx.log)?;
        }
    }
    cx.log.sync().await?;

    cx.queue = cx.plugins.service::<dyn MessageQueue>()?.attach(&id).await?;
    cx.phase = Phase::Idle;
    Ok(rt.spawn_agent(cx))
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

Recovery uses DeepSeek's honest wording for an interrupted tool and opencode's optional automatic continuation with a resume cap. Whether a crashed turn continues by itself is the per-agent `resume.after_crash` setting: `wait` or `continue { max }`. It is a required field with no implicit default, so each deployment decides. If an agent crashed while quiescing, before its image was handed over, it is recovered from the log like any other crash.

### Changing configuration

Configuration is pinned in the header and carried in every image, so a thawed or recovered agent behaves the way it did before. Changing it is a `Reconfigure` command. The driver applies it at the next safe point by appending a `Reconfigured { patch }` event and reloading the affected plugins; new plugin instances receive their state through `migrate` or start fresh, as their runner decides. Because the patch is in the log, recovery replays it, and anything it changes in the model's input is reconstructible.

### Side effects outside the log

The log cannot undo a file edit or a sent message. Two hooks help: tool plugins receive the call id in `ToolCx` and are expected to use it as an idempotency key where the target supports one, and a workspace plugin can take a file snapshot in `pre_step` (opencode's approach) so the edits of an interrupted turn can be reverted. Background processes started by tools are recorded as events with their process ids, so a resumed agent can report or stop them instead of losing track of them.

## Quiesce and graceful shutdown

None of the three surveyed harnesses can finish the current step or turn and then stop cleanly; each one's shutdown cuts the turn off, and a resumed session looks like a crash. Here quiesce is a phase of the agent.

```rust
pub enum DrainPoint {
    Step,       // stop at the next step boundary; the turn is suspended and continues on thaw
    Turn,       // let the current turn finish; stop before the next one
    Immediate,  // cancel the current step now; the log is closed like a cancel, then quiesced
}

pub enum DrainResult { Quiesced { image: AgentImage, forced: bool }, Closed }
```

What happens on `Command::Drain { at, deadline }`:

1. The phase becomes `Draining`. The agent stops pulling from its queue, so new messages wait there, and no new turn starts.
2. `drain_requested` is dispatched. Plugins that own background work (jobs, subagents, schedulers) stop admitting new work and report what must finish first. A subagent is itself an agent, so the parent drains its children with the same command.
3. The driver runs until the requested safe point. A step that is waiting on a tool keeps waiting until the tool finishes or the deadline passes.
4. At the safe point the driver appends `Quiesced`, syncs the log, serializes the context into an `AgentImage`, releases the log lease, detaches from its queue (leaving every unpulled message for whoever thaws the agent), and returns `Exit::Quiesced`. The handle's waiters receive the image.
5. If the deadline passes first, the driver falls back to `Immediate`: it cancels running tool bodies through their child tokens, writes "not run" or "outcome unknown" results the same way crash repair would, and then quiesces. The result carries `forced: true` so the operator knows the stop was not clean.

```rust
async fn settle_drain(cx: &mut AgentCtx, here: DrainPoint) -> Result<Option<Exit>, DriverError> {
    let Phase::Draining { at, deadline, .. } = &cx.phase else { return Ok(None) };
    let forced = Instant::now() >= *deadline;
    if !(here.satisfies(*at) || forced) { return Ok(None); }

    let suspended_turn = cx.current_turn_if_suspended();
    cx.log.append(Event::Quiesced { at: here, suspended_turn })?;
    cx.log.sync().await?;

    dispatch(cx, Hook::Snapshot).await?;              // [hook] opaque plugin state into slots
    let image = cx.to_image(suspended_turn)?;         // every slot, history, inbox, counters, log position
    cx.log.release_lease().await?;
    cx.queue.detach().await?;                         // unpulled messages stay for the next consumer
    cx.phase = Phase::Quiesced;
    cx.notify_drain_waiters(DrainResult::Quiesced { image, forced });
    Ok(Some(Exit::Quiesced))
}
```

What the caller does with the image is outside the harness: write it to local disk for a restart on the same host, or hand it to whatever moves agents between hosts.

A process-level graceful shutdown is then a drain of every agent with one deadline: on SIGTERM the runtime sends `Drain { at: Turn, deadline }` to all handles, collects the images, hands them to its configured image sink, and exits. Starting again is `thaw` on each image, which continues any suspended turn with no repair and no user message.

**Parking.** The same mechanism lets the runtime evict an agent that is waiting on something slow, such as a human approval in `tool_pre`. The agent appends `Parked { waiting_on }`, produces an image, and its task exits; messages sent meanwhile wait in its queue. An `Answer` command for that request makes the runtime thaw the image and deliver the answer. In a multi-tenant process this keeps memory proportional to agents that are doing work rather than agents that exist. If a drain reaches an agent that is parked, it stays parked: the pending request is neither denied nor answered, and it is still waiting when the agent is thawed, on this host or another.

## Message queue

Every message for the model, from a user, another agent, a scheduler or a webhook, is published to the agent's queue, and the agent pulls it when it is ready. The queue is a service provided by a plugin, so its backend is the plugin's code: a broker shared by the whole cluster in production, an in-process channel in tests.

```rust
pub trait MessageQueue: Plugin {
    /// Become the only consumer of this agent's queue. Fails while another consumer holds it.
    async fn attach(&self, agent: &AgentId) -> Result<QueueConsumer, QueueError>;
}

pub trait QueueConsumer: Send {
    /// Up to `max` messages, oldest first; waits for at least one when `wait` is set.
    async fn pull(&mut self, max: usize, wait: bool) -> Result<Vec<QueuedMessage>, QueueError>;
    /// Remove messages from the queue. Unacknowledged messages are redelivered to the next consumer.
    async fn ack(&mut self, upto: &QueueMsgId) -> Result<(), QueueError>;
    /// Stop consuming. Anything not acknowledged stays in the queue.
    async fn detach(self) -> Result<(), QueueError>;
}

pub struct QueuedMessage {
    pub id: QueueMsgId,          // assigned by the queue, increasing within one agent's queue
    pub sent_at: Timestamp,      // when the sender sent it; recorded in the log, not used for ordering
    pub delivery: Delivery,      // NextTurn | NextStep
    pub body: MessageBody,
}

/// Called by the driver when idle (wait) and at each step boundary (no wait).
/// Never pulls while the agent is draining.
async fn pull_messages(cx: &mut AgentCtx, mode: Pull) -> Result<(), DriverError> {
    if cx.phase.is_draining() { return Ok(()); }
    let batch = cx.queue.pull(cx.config.queue.batch, mode == Pull::Wait).await?;   // [config]
    let Some(last) = batch.last().map(|m| m.id.clone()) else { return Ok(()) };
    for msg in batch {
        if msg.id <= cx.inbox.last_logged { continue; }          // redelivered after a crash: already logged
        cx.log.append(Event::MessageReceived(msg.clone()))?;
        cx.inbox.last_logged = msg.id.clone();
        cx.inbox.push(msg);
    }
    cx.log.sync().await?;            // in the agent's log...
    cx.queue.ack(&last).await?;      // ...before it leaves the queue
    Ok(())
}
```

Three properties come from this ordering.

1. **Nothing is lost.** A message leaves the queue only after it is durable in the agent's log. If the agent crashes in between, the queue redelivers it, and the agent recognizes it by its id because `last_logged` is rebuilt from the log.
2. **Nothing arrives while quiesced.** A draining agent stops pulling, and a quiesced agent is detached. Messages wait in the queue in order, so the agent's own state never has to hold or sort messages it has not asked for.
3. **One consumer at a time.** The queue plugin grants one consumer per agent queue, and the session store's epoch backs this up: a stale consumer cannot write `MessageReceived`, so it never acknowledges, and its messages go to the current owner.

Duplicates sent by senders themselves, such as a client retrying a send, are the queue's concern. Most brokers can deduplicate on a sender-supplied message id within a window, and a queue plugin can expose that; the agent assumes that what it pulls is what was meant to be delivered.

## Migrating an agent between hosts

Resizing a cluster moves agents by quiescing them on one host and thawing them on another. Messages wait in the shared queue and the log store is reachable from every host, so the move is only the image changing hands.

1. Host A is running the agent and pulling from its queue.
2. The orchestrator sends `Drain { at, deadline }` to the agent on A. It stops pulling and runs to the safe point.
3. A appends `Quiesced` to the log, serializes the context into an image, releases the log, and detaches from the queue. Messages sent from now on wait in the queue.
4. The image is transferred to host B. How is outside the harness.
5. B calls `thaw(image)`. It takes ownership of the log (raising the epoch), rebuilds the context from the image, attaches to the queue and runs. It continues any suspended turn, then pulls the messages that arrived during the move.
6. Host A can be torn down at any point after step 3.

```rust
/// Orchestrator side. Each call is idempotent so the move can be retried.
async fn migrate(id: AgentId, a: &HostClient, b: &HostClient, cfg: &MigrationConfig) -> Result<(), MigrateError> {
    let image = a.drain(id, cfg.drain_point, cfg.drain_deadline).await?;   // steps 2-3
    b.thaw(image).await?;                                                  // steps 4-5
    Ok(())
}
```

The image records the log position it is consistent with, and thaw refuses to continue a log that has moved past it, so an out-of-date image can never fork the conversation. If B fails during thaw, the same image can be thawed on another host; the epoch rises again and nothing on B can append. If A crashed before producing an image, any host can `recover` the agent from its log.

## Creating an agent

Every agent's configuration is plugin-supplied, which raises the question of where the first configuration comes from. The answer is one runtime-level plugin, the **provisioner**, named in the process's startup file together with the runners the process offers. It also names the log store, which is part of the hosting infrastructure in the same way. That file is the only configuration that is not produced by a plugin.

```toml
# runtime.toml: read once at process start
[runners]
enable = ["builtin.*", "wasm", "process.python", "mcp"]

[log_store]
runner = "builtin.logstore.s3"
code.inline = { bucket = "agent-logs", prefix = "{tenant}/{agent}" }

[provisioner]
runner = "process.python"
code.package = { name = "acme-provisioner", version = "1.0.4", digest = "sha256:51ab..." }
grants = { hosts = ["tenants.internal"], secrets = ["secret://platform/tenant-db"] }
```

```rust
/// The one plugin loaded per process instead of per agent. It runs in its own
/// small context with its own grants and never sees any agent's state.
pub trait Provisioner: Plugin {
    /// Produce everything needed to build a new agent.
    async fn provision(&self, req: CreateRequest) -> Result<AgentSpec, ProvisionError>;
}

pub struct CreateRequest {
    pub tenant: TenantId,
    pub template: String,          // e.g. "support", "coder"
    pub params: toml::Table,       // caller-supplied values the template may use
}

pub struct AgentSpec {
    pub id: AgentId,
    pub config: AgentConfig,                     // every plugin: runner, code, grants
    pub initial_state: Vec<(PluginId, StateBlob)>,
    pub initial_events: Vec<Event>,              // e.g. seed instructions or a first message
}

pub async fn create(rt: &Runtime, req: CreateRequest) -> Result<AgentHandle, CreateError> {
    let spec = rt.provisioner.provision(req).await?;
    let log = rt.log_store.open(&spec.id, OpenMode::Create).await?;   // fails if the agent exists

    // Same build path as thaw and recovery: load every plugin through its runner.
    let mut cx = AgentCtx::build(rt, &spec.config, log).await?;
    cx.log.write_header(&spec.config)?;
    for (pid, blob) in spec.initial_state {
        cx.state.insert(&pid, cx.plugins[&pid].decode_state(blob)?);
    }
    for event in spec.initial_events { cx.log.append(event)?; }       // also updates history
    cx.log.sync().await?;

    cx.queue = cx.plugins.service::<dyn MessageQueue>()?.attach(&cx.id).await?;
    cx.phase = Phase::Idle;
    Ok(rt.spawn_agent(cx))
}
```

The provisioner is consulted once per agent. After `create`, the agent's configuration travels in its images and is pinned in its log header, so thaw and recovery never call the provisioner. A configuration change after creation is a `Reconfigure` command, which the provisioner can issue like any other caller, and which is logged and carried in later images.

Because the provisioner is a plugin like any other, it can be as simple or as elaborate as a deployment needs: `builtin.provisioner.templates` whose code is a directory of TOML templates with tenant variables filled in, or a `process.python` application that reads tenant settings from a database. A provisioner that is unavailable stops new agents from being created, but it does not affect running, quiescing or thawing agents.

## Isolation in a multi-tenant process

Isolation has four layers, from the language up to the operating system.

**State.** All mutable per-agent state is in `AgentCtx`, owned by one task. Plugin instances are immutable and runners hold only tenant-neutral resources. The rules that keep it that way are enforced in review and, where possible, by lints: no `static` with interior mutability in runner crates, no `thread_local!` or `task_local!` carrying agent data, no `Mutex` inside a plugin instance. Because instances are immutable, two agents with identical plugin config could share one instance safely (the equivalent of DeepSeek's shared preset generations); that is an optimization to adopt only after measuring.

**Capabilities.** A plugin can reach nothing it was not granted. `PluginGrants` come from the agent config and name the services it may call, the hooks it may subscribe to, filesystem roots, network hosts, and secrets. Secrets are references (`secret://tenant/name`) resolved through the runtime per call, never stored in the log or in state slots. Tools receive a `ToolCx` with only that call's grants. Shared resources such as an HTTP connection pool are safe only if credentials travel with each request, which is the rule for every runner.

**Faults and resources.** Each agent task runs under `catch_unwind` at its top level; a panic in a plugin fails that agent, writes a crash marker if possible, and leaves the process and other agents running. Model calls and tool bodies acquire slots from the tenant's `QuotaHandle`, so one tenant cannot starve the rest. WASM plugins run with per-instance fuel and memory limits. A quota breach pauses or fails the agent; it never aborts the process.

**Code.** The runner decides how far untrusted code is from the host:

- `builtin` code is trusted Rust, configured by data. It is as isolated as the context and grants make it.
- `wasm` code runs in the host but in its own linear memory, one instance per plugin per agent. This is the preferred way to run tenant-supplied code in-process.
- `process` code runs in a separate OS process with the harness sandbox applied. Embedding CPython or a JS engine directly in the host is ruled out for tenant code: interpreter state and native extensions are shared across everything in the process, which breaks the isolation requirement.

## Worked example

Two agents in the same process, owned by different tenants and configured differently. Each is one TOML document, as a provisioner might return it; the runtime records it in the agent's log header at creation. Neither names a log store, because that is set once for the process.

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

[snapshot]
every_steps = 20   # periodic snapshots, to shorten crash recovery for long sessions

[[plugin]]
id = "queue"
runner = "builtin.queue.nats"
code.inline = { url = "nats://queue.acme.internal:4222", stream = "agents" }
grants = { hosts = ["queue.acme.internal"] }

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

[[plugin]]
id = "queue"
runner = "builtin.queue.nats"
code.inline = { url = "nats://queue.globex.internal:4222", stream = "agents" }
grants = { hosts = ["queue.globex.internal"] }
```

What the loop does with B's Python plugin:

- At creation, the `process.python` runner fetches the package by digest, starts a child process for this agent, and asks it which hooks it handles. It may subscribe only to the granted hooks; anything else fails creation.
- On `pre_step`, the runner sends the step input as JSON. The Python code returns context to add (for example, the customer's open tickets); the runner appends it as an event, so the model sees it and crash recovery reproduces it.
- On `tool_pre` for `open_ticket`, the Python code returns `ask`, so the driver asks a human and the agent parks until the answer arrives.
- On `snapshot`, the Python code returns its own state as JSON, which goes into the agent's image. On thaw, a new child process, possibly on another host, receives it before the agent runs.
- If the Python process dies, the runner reports a `PluginError` on the next hook, which fails B's current turn; A is unaffected.

## Decisions and open questions

Decided after review of the first draft:

1. **Behavior after a crash is configuration.** `resume.after_crash` is a required per-agent field, `wait` or `continue { max }`, with no implicit default.
2. **Crash recovery replays the log; thaw does not.** Periodic snapshots are optional and off by default, since slower recovery after a crash is acceptable. Quiesce serializes the whole context into an image, and thaw rebuilds the agent from that image alone; messages sent meanwhile are still in the queue.
3. **One hook definition for every non-Rust runner.** The hooks are defined once as WIT, and the JSON-RPC schema for `process` runners and the Rust bindings are generated from it.
4. **`process` plugins run one process per agent.**
5. **A drain does not deny pending approvals.** A parked agent stays parked through a quiesce or a move, and the request is still waiting when it resumes.
6. **The log store is hosting infrastructure.** It is a runtime-level plugin named in the process's startup file, the same for every tenant, and need not be local.
7. **Inbound messages wait in a shared queue, provided by a plugin.** An agent pulls only when it can use a message, so nothing arrives while it is quiesced, and moving it between hosts is a quiesce, a transfer of the image, and a thaw.
8. **Messages are used in queue order.** The agent never sorts or reorders them. Each message carries the time it was sent, which is logged for diagnosis but not used for ordering.
9. **New agents come from a provisioner plugin** named in the process's startup file. After creation, an agent is rebuilt from its own log.

Nothing is open from the review so far.

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
| Runner and code split, immutable plugin instances, state slots, drain and park phases | New in this proposal |
| Pull-based message queue plugin, epoch fencing for moves between hosts, provisioner plugin for initial configuration | New in this proposal |
