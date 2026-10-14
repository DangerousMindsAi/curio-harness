# DeepSeek Harness: the agent loop

Snapshot: [DangerousMindsAi/deepseek-harness](https://github.com/DangerousMindsAi/deepseek-harness) at commit `d743267`. All paths are relative to the repository root. "Inferred" marks a claim not read directly from code.

The loop is a single driver class, `ReactLoopAgent`, that drains a durable two-list inbox into turns, and each turn into steps, where a step is one model request plus the tool calls that response makes (`packages/core/agent-loop/src/agent.ts:97-705`). Every model request is derived from the append-only session log immediately before it is sent, so every input the loop admits is first appended as a session event. Almost all policy (retry, compaction, context injection, permission, timeouts, continuation) lives in plugins attached to a small set of Cordis waterfalls and serial events that the driver and the tool registry dispatch at fixed points.

## Pseudo-code

```text
# ---- inbox (agent.ts:154-181, inbox.ts:109-243) ----
followup(msg): send(msg, 'next-turn', wake=true)     # a queued turn
steer(msg):    send(msg, 'next-step', wake=true)     # nearest step boundary
inject(msg):   send(msg, 'next-step', wake=false)    # waits for a waking message
send(msg, target, wake):
    if wake and activity already aborted: target = 'next-turn'
    inbox.splice(target, end, 0, [msg])               # log: agent/inbox/spliced
                                                      # event: agent/inbox/inserted (emit)
    if wake: wakeDriver()     # idle -> phase 'running' (emit: agent/status), start kick()
                              # maintenance or aborted -> latch wakeRequested, replay at idle
cancel(cause, keepInbox?): clear inbox unless keepInbox; abort(cause)

kick(): while turn(): pass                # runs until the inbox is empty; errors contained

# ---- turn (agent.ts:298-398) ----
turn():
    append turn/start {turn}                                  # log: turn/start
    turnEnds = null; target = 'next-turn'
    loop:
        decision = preStep(target)
        if decision.reject: turnEnds = blocked; break
        if turnEnds and decision.messages empty: break
        if first step and decision.messages empty: turnEnds = completed; break   # no model call
        append step/start                                     # log: step/start
        recovery = ToolCallRecovery observing session/event
        try:   stepEnd = step(decision)
               if turnEnds is not max-tokens: turnEnds = stepEnd   # max-tokens is sticky
        except: append synthetic tool/result for owed calls  # log: tool/result
                raise
        finally: append step/end                              # log: step/end
        if turnEnds and inbox.nextStep empty:
            await serial agent/turn-stopping {turn, signal}   # serial: listeners may steer()
        if turnEnds and inbox.nextStep empty: break
        target = 'next-step'
    on abort:  turnEnds = aborted{cause: user|parent|hook|disposed}
    on error:  turnEnds = error{LlmFailure | {UNKNOWN, errorChain}}; emit agent/error
    finally:   append turn/end {turn, reason: turnEnds}       # log: turn/end
    return inbox.hasPending                                   # another turn if input is queued

preStep(target):                                              # agent.ts:267-288
    claimed = inbox.claim(target)     # all next-step, plus ONE next-turn msg if target is next-turn
                                      # log: agent/inbox/spliced; emit: agent/inbox/claimed
    assembly = systemPrompt.assemble()                        # waterfall: system-prompt/assemble
    return waterfall agent/pre-step {messages: claimed, turn, step, signal}
        default (innermost): enter(claimed + runtime-context snapshot if changed)
        # listeners: reject | enter(messages', startsRequestSeries?)

# ---- step (agent.ts:400-561) ----
step(decision):
    firstAttempt = true
    loop:                                                     # one iteration per request attempt
        config = waterfall agent/request {turn, step, signal} # seed: AgentOptions, then logged header
        prepared = llm.prepareCall(config)                    # service: ctx.llm; NO_ADAPTER tolerated
        context = refresh runtime-context sections
        for commit in SystemPromptProjection.project(prompt, inHistory=prepared.systemPromptUpdate,
                                                     startsSeries=...):
            append system/message (append or replace)         # log: system/message
        if firstAttempt: append each decision message         # log: user/message
        append runtime-context snapshot if not yet admitted   # log: user/message
        firstAttempt = false
        request = buildRequest():                             # agent.ts:616-704
            append request/header {initial|resume|change|series} when needed   # log: request/header
            append developer/message {tool-addition|tool-removal} on tool delta # log: developer/message
            append request/context when provider/model/window changed          # log: request/context
            messages = deepFreeze(session.deriveMessages())   # model input = projection of the log
        stream = prepared.stream(request)                     # waterfall: llm/stream (inside ctx.llm)
        emit agent/assistant-stream start; for chunk: emit chunk   # emit, process-local only
        if stream throws (after start):
            append assistant/message{interrupted} if aborted with content,
            else assistant/attempt                            # log: assistant/attempt
            raise
        if finish is error or aborted:
            append assistant/attempt                          # log: assistant/attempt
            action = waterfall agent/request-error {failure, retryPolicy, signal}
            if action is retry: continue                      # re-runs agent/request, NOT pre-step
            raise LlmError(failure)
        append assistant/message {message, usage, stream}     # log: assistant/message
        if finish is max-tokens: return max-tokens            # tool calls are NOT dispatched
        if no tool-call blocks: return completed
        concluded = executeToolCalls(calls, acceptContext = inbox.splice('next-step', ...))
        return completed if concluded else null               # null = model owes another step

# ---- tool dispatch (tool-calls.ts:60-243) ----
executeToolCalls(calls):
    while calls remain:
        mode = tools.executionMode(first)   # isConcurrencySafe(args) === true -> parallel, else exclusive
        group = parallel ? rest of calls : [first]            # exclusive call = barrier of one
        runGroup(group)
        if aborted: for each unstarted call:
            append tool/call + tool/result ABORTED_BEFORE_DISPATCH   # log: tool/call, tool/result
runGroup(group):
    max = agentLoop.config.maxParallelToolCalls.get()         # config: maxParallelToolCalls (volatile)
    fillPool: while not aborted and inFlight < max:
        if next call now classifies exclusive: stop filling   # re-classified before each start
        append tool/call                                      # log: tool/call
        prepared = await scheduler.prepare(exec)              # pre-execute stage, in model order
        if prepared is dispatch: inFlight += scheduler.dispatch(exec)   # bodies overlap
        commitReady()
    while inFlight: await first settled; commitReady(); refill
    commitReady(): for contiguous settled slots in model order:
        result = finalize (post-execute stage) or finish
        append tool/result {sourceEventSeqs:[tool/call seq]} # log: tool/result
        for ctx in result.additionalContexts: inbox.splice('next-step')  # log: agent/inbox/spliced
        concluded |= result.concludesTurn
    on scheduler failure: drain started calls, raise (turn records recovery results)

# ---- per-call pipeline (packages/core/tools/src/index.ts:1499-1910) ----
prepare(exec):
    snapshot + freeze arguments; ptc-mode collapse denies model-direct non-run_code names
    if caller aborted: return ABORTED_BEFORE_DISPATCH
    d = waterfall tools/pre-execute (exec)                    # default allow; allow|deny|cancel|ask
    if d is ask: d = approval.request(...)                    # service: ctx.approval; absent -> deny
                                                              # log (by approval plugin): approval/asked, approval/decided
    if d is cancel: return post-result ABORTED_BEFORE_DISPATCH
    reason = d is allow ? first guard denial : d.reason       # tools.guard(): monotonic, deny-only
    if reason: return post-result {isError, "Error: <reason>"}   # still flows through post-execute
    return dispatch
dispatch(exec):
    r = waterfall tools/execute (exec)                        # around-dispatch: timeout, checkpoint
          innermost: tool.execute(args, exec) -> validate output schema -> render content
    add deferred contexts; caller abort turns success into ABORTED
finalize(exec, r):
    r = ToolDefinition.projectContent(r)
    decision = waterfall tools/post-execute (exec, r)         # accept | accept{content|value} | block
    finish(r)
finish(exec, r): materialize, ToolDefinition.finalizeContent(r), emit tools/result (frozen)

# ---- stop conditions ----
# A turn ends when a step completes with no tool calls, a tool result sets
# concludesTurn, a response hits max-tokens, agent/pre-step rejects, the first
# claim is empty, the signal aborts, or an error escapes. Each requires an empty
# next-step inbox after agent/turn-stopping. There is no step or turn cap.
```

Source map:

- Inbox commands and wake/latch: `packages/core/agent-loop/src/agent.ts:154-235`; durable inbox and claim: `packages/core/agent-loop/src/inbox.ts:109-243`.
- Driver loop: `packages/core/agent-loop/src/agent.ts:252-265`.
- Turn: `packages/core/agent-loop/src/agent.ts:298-398`; pre-step: `agent.ts:267-288`.
- Step, request attempt and retry: `packages/core/agent-loop/src/agent.ts:400-561`; `prepareRequest`: `agent.ts:564-613`; `buildRequest`: `agent.ts:616-704`.
- System prompt and runtime-context projection: `packages/core/agent-loop/src/runtime-context.ts:88-103` and `:152-163`.
- Stream framing: `packages/core/agent-loop/src/assistant-stream.ts:49-109`.
- Tool scheduler: `packages/core/agent-loop/src/tool-calls.ts:60-243`; synthetic results: `tool-calls.ts:250-260`.
- Concurrency classification: `packages/core/tools/src/index.ts:1309-1319`.
- Per-call pipeline: `packages/core/tools/src/index.ts:1499-1550` (pre-execute, approval, guards), `:1570-1640` (execute), `:1647-1700` (post-execute, finalize, `tools/result`), `:1733-1776` (approval), `:1787-1830` (post-execute decisions).
- Event declarations: `packages/core/agent/src/runtime-types.ts:250-394`; `tools/*` declarations: `packages/core/tools/src/index.ts:143-208`.

## What is configurable

The loop has very few knobs of its own. Most loop behavior changes come from which plugins are composed and from their configs. Configuration is the composed `cordis.yml` tree (bundle patches, then the profile patch, then the home patch, then `--patch`); fields declared `.volatile()` are also editable through `dsh-settings` without restart.

| Knob | Where it is set | Default | Effect on the loop |
|---|---|---|---|
| `maxParallelToolCalls` | `agent-loop` row config; volatile, so also a user setting (`packages/core/agent-loop/src/index.ts:346`) | 10 (`packages/core/agent-loop/src/constants.ts:6`) | Caps in-flight parallel-safe tool bodies per group. Read once per group, so a change applies to the next group (`tool-calls.ts:132`). `1` makes dispatch serial. |
| `agents[]` (`id`, `sessionId`, `resumeSessionId`, `cwd`, `provider`, `model`, `reasoningEffort`, `maxTokens`) | `agent-loop` row config (`packages/core/agent-loop/src/index.ts:347-356`) | `[]` in base (`packages/bundle/base/cordis.patch.yml:499-502`) | Agents created or resumed at startup. `provider`/`model`/`reasoningEffort`/`maxTokens` seed the first `agent/request` proposal (`agent.ts:575-591`). `maxTokens` must be a positive safe integer (`index.ts:202-207`). |
| `AgentOptions` per create/resume | Programmatic (`ctx.agents.create`, SDK, Web session controller) | none | Same seed as above for that agent. |
| Default model route | `agent-default-model` row: `provider`, `model`, `reasoningEffort`, all volatile (`packages/core/agent-default-model/src/index.ts:51-55`) | `deepseek-official` / `deepseek-flash` (`packages/bundle/base/cordis.patch.yml:85-89`) | Read by entry points when they create agents; it reaches the loop as `AgentOptions` (inferred from the row comment at `base/cordis.patch.yml:83-84`). |
| Tool presentation `mode` | `tools` row config `mode: native \| ptc` (`packages/core/tools/src/index.ts:812`); env `DSH_TOOLS_MODE` in headless and web patches (`packages/bundle/headless/cordis.patch.yml:17`, `packages/bundle/web-app/cordis.patch.yml:38`); per agent through `dsh-agent-tool-presentation` (`packages/core/agent-tool-presentation/src/index.ts:51`, used by the `ptc` preset at `packages/bundle/web-app/presets/ptc.patch.yml:135-138`) | `native` | `ptc` replaces the tool list in each request with `run_code` and denies model-direct calls to other names before the pre-execute waterfall (`packages/core/tools/src/index.ts:1357-1359`, `:1397-1481`). |
| `maxParallelSubCalls` | `tools` row config (`packages/core/tools/src/index.ts:813`) | 10 | Concurrency of a `run_code` program's sub-calls; same exclusive/parallel rules. |
| Provider `retryPolicy` | Per provider route, for example `llm-deepseek` config, volatile (`packages/llm/llm-deepseek/src/config.ts:100`); pi-ai profiles (`packages/llm/llm-pi-ai/src/config.ts:182`) | `normal`, 5 retries, 500 ms initial, 10 s max, jitter 0.1, codes `EMPTY_RESPONSE`, `RATE_LIMIT`, `SERVER`, `TIMEOUT`, `TRANSPORT` (`packages/llm/llm/src/retry-policy.ts:14-24`); `always` mode retries every failure | Read by `llm-retry` in `agent/request-error`. `llm-retry` itself takes no config and rejects a `retryPolicy` key (`packages/llm/llm-retry/src/index.ts:25-38`). |
| Compaction policy | `compaction-basic` row: `auto`, `thresholdRatio`, `retainRatio`/`retainTokens`, `headroomTokens`, `maxTokens`, `summarizationProvider`/`Model`, `compactionRetries`, `maxOverflowRetries`, `modelPolicies[]` (`packages/compaction/compaction-basic/src/config.ts:26-43`) | `auto: true`, 0.8, 0.16, 65536, retries 1, overflow retries 1 (`config.ts:20-23`, `:75-107`) | `auto: false` removes both the pre-step and request-error listeners (`packages/compaction/compaction-basic/src/index.ts:140`). |
| Tool-result pruning | `tool-result-pruner` row: `thresholdChars`, `headChars`, `tailChars` | 8192 / 4096 / 1024 (`packages/bundle/base/cordis.patch.yml:420-425`) | First, model-free phase of compaction. |
| Spill | `spill-policy` row `maxInlineTokens` (`packages/spill/spill-policy/src/index.ts:27-34`) | 12500 in base (`base/cordis.patch.yml:409-412`) | Rewrites oversized results in `tools/post-execute`. |
| Repeat reminders | `repeat-tool-reminder` row: `thresholds`, `include`, `exclude`, `argumentsPreviewChars` (`packages/guard/repeat-tool-reminder/src/index.ts:53-56`) | `[3, 5, 8]`, all tools, 500 | Adds a reminder `additionalContexts` message at those consecutive identical-call counts. |
| Tool timeout | `ToolDefinition.timeoutMs` per tool (`packages/core/tools/src/index.ts:267`); `timeout-policy` has no config | none per tool unless declared | `timeout-policy` arms a deadline in `tools/execute` and maps expiry to `TOOL_TIMEOUT` (`packages/guard/timeout-policy/src/index.ts:56-80`). |
| Goal round cap | `goal` row `defaultMaxGoalRounds` (`packages/goal/goal/src/index.ts:244`) | 256 | The goal driver blocks the goal with `round-limit` instead of queuing another turn (`packages/goal/goal-round-driver/src/index.ts:166-171`). |
| Approval policy | `approval` row `policy: ask \| never`; base derives it from env `DSH_PERMISSION_MODE` (`packages/bundle/base/cordis.patch.yml:254-257`) | `ask` | Governs `ctx.approval` used for pre-execute `ask` and in-tool sandbox escalation. |
| Sandbox mode | `sandbox-policy` row `mode`, env `DSH_PERMISSION_MODE` (`base/cordis.patch.yml:238-242`) | `workspace-write` | Not a loop knob; it decides what tool bodies may do and when they request escalation through approval (`packages/sandbox/sandbox/src/escalation.ts:27-30`). |
| Job completion delivery | `tool-jobs` row `completionDelivery: wakeup \| quiet`, `maxConsecutiveWakes` (`packages/jobs/tool-jobs/src/index.ts:62-63`) | `wakeup`, uncapped | `wakeup` calls `followup()` on an idle owner, which opens a new turn; otherwise `inject()` (`tool-jobs/src/index.ts:295-307`). |
| Default preset | `agent-preset-registry` row `default` (`packages/bundle/web-app/cordis.patch.yml:574-577`) | `standard` (Web only) | Selects which plugin set (tools, compaction, plan mode) each new Web agent binds to. |
| Per-agent tool filter | `tools.restrict({allow, deny})` on a scoped context (`packages/core/tools/src/index.ts:1103-1131`); subagent `toolFilter` config (`packages/subagent/subagent/src/child-agent.ts:216`) | none; Web's standard preset denies `schedule_*` to children (`packages/bundle/web-app/presets/standard.patch.yml:99-115`) | Removes tools from that agent's prompt and refuses their execution. |

There is no step cap, turn cap, token budget, or wall-clock budget in the loop; grep finds no `maxSteps` or `maxTurns` under `packages/`. A turn continues while the model keeps making tool calls.

## Plugins and hooks

The loop is itself a replaceable plugin. `AgentLoop` is a Cordis `Service` under `ctx.agentLoop` that installs itself as the agent factory with `ctx.agents.setFactory(this)` (`packages/core/agent-loop/src/index.ts:341-388`), and `ReactLoopAgent` implements the `Agent` interface declared in `dsh-agent`. The extension points below are declared in `dsh-agent` and `dsh-tools`, not in the loop package, so a different driver can dispatch the same events. Agent events are scope-filtered: a listener registered through an agent's scoped context receives only that agent (`packages/core/agent/src/runtime-types.ts:317`). Waterfall listeners must call `next()` to delegate; returning without it short-circuits the chain (`docs/cordis-primer.md:29-35`).

### Loop extension points

| Event | Mode | When | What a listener can do |
|---|---|---|---|
| `agent/created` | serial | After factory setup, before queued input runs; the agent is in a maintenance phase so wakes latch (`packages/core/agent/src/index.ts:550`, `agent-loop/src/index.ts:818-835`) | Initialize per agent; `inject()` context; a throw fails creation. |
| `agent/inbox/inserted`, `/claimed`, `/discarded` | emit | On each inbox mutation (`inbox.ts:112`, `:236-241`) | Observe only. |
| `agent/status` | emit | `idle` and `running` transitions (`agent.ts:145-152`) | Observe; plugins call `followup()` on idle to start a new turn. |
| `system-prompt/assemble` | waterfall | Inside pre-step assembly (`packages/core/system-prompt/src/index.ts:31`, `:674`) | Rewrite sections, variables and tool schemas for the next request. |
| `agent/pre-step` | waterfall | Before every step, after the inbox claim (`agent.ts:274-284`) | Reject the step (turn ends `blocked`), rewrite or append messages, set `startsRequestSeries`, or perform side work such as compaction or a checkpoint before the request. Does not run again on a retry. |
| `agent/request` | waterfall | Each request attempt, after `step/start` (`agent.ts:593-596`) | Replace the call config (provider, model, effort, max tokens). Cannot change messages. |
| `llm/stream` | waterfall | Around the adapter stream (`packages/llm/llm/src/index.ts:75`, `:1149-1156`) | Wrap or delay the stream; adapter throws become a terminal failure chunk. |
| `agent/assistant-stream` | emit | Start, each chunk, end of an attempt (`assistant-stream.ts:49-109`) | Observe live tokens; not durable. |
| `agent/request-error` | waterfall | After an attempt finishes with an `error` or `aborted` finish reason (`agent.ts:506-526`) | Return `{kind: 'retry'}` to re-run `agent/request` and the stream in the same step; `undefined` makes the failure terminal (turn ends `error`). |
| `agent/turn-stopping` | serial | When a step reports nothing owed and the next-step inbox is empty (`agent.ts:361-364`) | Call `agent.steer()` to force another step. No return value. |
| `agent/error` | emit | On a contained turn failure (`agent.ts:245-250`) | Observe. |

### Tool pipeline extension points

| Event / API | Mode | What a listener can do |
|---|---|---|
| `tools/pre-execute` | waterfall (`packages/core/tools/src/index.ts:152-154`) | `allow` (default), `deny` with reason, `cancel`, or `ask` (routed to `ctx.approval`). Cannot rewrite arguments (`index.ts:600-612`). |
| `tools.guard(fn)` | registered function, not an event (`index.ts:1142-1148`) | Deny only; evaluated after pre-execute and after approval, only when the decision is `allow` (`index.ts:1525`). |
| `tools/execute` | waterfall (`index.ts:163-165`) | Wrap the body: replace `exec.signal`, measure, retry, or substitute a result. |
| `tools/post-execute` | waterfall (`index.ts:175-177`) | `accept` (optionally replacing `content` or `value`), `block` (turn into an error with feedback), and attach `additionalContexts`, which the loop stages in the next-step inbox (`index.ts:1787-1830`; `tool-calls.ts:157`). Denied calls also pass through it. |
| `tools/result` | emit (`index.ts:197-199`) | Observe the frozen final result. |
| `ToolRunContext.concludeTurn()` / `deferContext()` | tool-body API (`index.ts:1428-1431`) | A tool ends the turn after its step, or queues context for the next step. |

### Which shipped plugins hook these

The shipped profile templates are `web` = base + web-app, `headless` = base + headless, `sdk` = base + sdk-app, `acp` = base + acp-app, and `sdk-minimal` = a standalone tree (`packages/boot/app-boot/src/profile.ts:179-195`). Base composes most policy rows globally. The web-app patch disables the base rows for tools, compaction, plan mode, agent instructions, todo and subagent tools and re-composes them per agent through presets (`packages/bundle/web-app/cordis.patch.yml:465-568`); `goal-round-driver`, `llm-retry`, `image-offload`, `repeat-tool-reminder`, `timeout-policy`, `spill-policy` and `session-checkpoint-policy` stay global in Web.

| Plugin | Composed by default in | Hooks | Effect |
|---|---|---|---|
| `dsh-llm-retry` | base (`base/cordis.patch.yml:94-95`), sdk-minimal | `agent/request-error` (`packages/llm/llm-retry/src/index.ts:243`) | For retryable codes, appends `llm/retry`, waits the backoff, appends `llm/retry-started`, returns `retry` (`packages/llm/llm-retry/src/index.ts:188-191`). Retry count is folded from the log per provider and policy. |
| `dsh-compaction-basic` | base (headless, sdk, acp); Web `standard`, `ptc`, `cordis` presets | `agent/pre-step` (`packages/compaction/compaction-basic/src/index.ts:158`), `agent/request-error` (`:190`) | Pre-step: compacts when context pressure crosses the threshold, then delegates. Request error: on `CONTEXT_WINDOW_EXCEEDED`, compacts and returns `retry` if the surface changed. |
| `dsh-compaction-tool-result-pruner` | base; Web presets | none; a service called by compaction-basic (`compaction-basic/src/index.ts:292-296`) | Model-free head/tail trimming before summarization. |
| `dsh-compaction-image-offload` | base | `agent/request-error` (`packages/compaction/compaction-image-offload/src/index.ts:27`) | On `IMAGE_OFFLOAD_REQUIRED`, records an `image/offload` decision replacing the oldest images, returns `retry` without spending retry budget. |
| `dsh-session-checkpoint-policy` | base | `llm/stream`, `tools/execute`, `agent/pre-step` (`packages/session/session-checkpoint-policy/src/index.ts:64-83`) | Flushes the session to disk before each model request and each top-level tool body; a failed flush fails closed. |
| `dsh-tool-call-timeout-policy` | base | `tools/execute` (`packages/guard/timeout-policy/src/index.ts:56`) | Cooperative per-tool deadline from `timeoutMs`. |
| `dsh-repeat-tool-reminder` | base | `tools/post-execute` (`packages/guard/repeat-tool-reminder/src/index.ts:220`), `agent/pre-step` (`:236`) | Counts identical consecutive calls, attaches a reminder; resets on user input. Advisory only. |
| `dsh-spill-policy` | base | `tools/post-execute` (`packages/spill/spill-policy/src/index.ts:133`) | Replaces oversized content with preview plus a file path. |
| `dsh-goal-round-driver` with `dsh-goal` | base (all base-backed profiles) | `agent/status`, `agent/created`, inbox events, `session/event`, `agent/pre-step` (`packages/goal/goal-round-driver/src/index.ts:252-429`) | When the agent goes idle with an active, armed goal, queues a goal prompt with `followup()` (`:192`). Pre-step rejects a stale round or marks a valid one `startsRequestSeries` (`:428`). |
| `dsh-plan-mode` | base; Web `standard` preset | `agent/pre-step` (`packages/plan/plan-mode/src/index.ts:196`), plus a prompt section | Appends `plan/mode` and a narration message at a step boundary. Does not restrict tools; `exit_plan_mode` calls `concludeTurn()` when the review is dismissed (`plan-mode/src/index.ts:337`). |
| `dsh-user-approval` | base | service `ctx.approval`, used by `tools/pre-execute` `ask` and by tool bodies for sandbox escalation (`packages/core/tools/src/index.ts:1733-1776`; `packages/shell/tool-bash/src/index.ts:238`) | One-shot approval with `approval/asked` and `approval/decided` audit events; `never` policy and absent channels resolve without a human. |
| `dsh-tool-jobs` | base; Web presets | `tools/pre-execute` (`packages/jobs/tool-jobs/src/index.ts:220`), job completion listener | Opens a new turn for job completions on an idle owner (`:295-307`). |
| `dsh-agent-instructions`, `dsh-time-context`, `dsh-tool-skill` | base or presets | `agent/pre-step` (`packages/context/agent-instructions/src/index.ts:334`, `packages/context/time-context/src/index.ts:185`, `packages/skill/tool-skill/src/index.ts:179`, `:215`) | Append context messages (AGENTS.md chains, time, skill catalog, invoked skills) to the entered batch, so they are logged as `user/message`. |
| Model selection | installed per agent by Web session controller, headless and ACP (`packages/api/session-controller/src/agent.ts:323`, `packages/bundle/headless/src/index.ts:342`, `packages/acp/acp/src/model-control.ts:59`) | `system-prompt/assemble`, `agent/request`, `agent/pre-step` (`packages/core/agent/src/model-selection.ts:82-127`) | Applies the user's picked model to the next request and appends a model-switch notice. |
| `dsh-workspace-changes` | web-app (inferred from the `workspace-changes` row at `packages/bundle/web-app/cordis.patch.yml:352-353`) | `agent/turn-stopping`, `tools/pre-execute` (`packages/deliverables/workspace-changes/src/index.ts:155`, `:174`) | Settles file-change recording before the turn closes; does not steer. |
| Archived-session gate | Web session controller (`packages/api/session-controller/src/index.ts:173`) | `agent/pre-step` (`archived-session-gate.ts:27-31`) | Rejects steps for archived sessions, so the turn ends `blocked`. |
| `dsh-fs-observation-policy` | base | `fs/write-intent`, `fs/edit-intent` (below the tool layer) | Read-before-write checks inside `tool-fs` bodies. |
| Hooks bridges (`dsh-hooks-claude-code`, `dsh-hooks-codex`) | not composed by any shipped bundle; opt-in experimental | `agent/created`, `agent/pre-step`, `tools/pre-execute`, `tools/post-execute`, `agent/turn-stopping` (`packages/experimental/hooks-claude-code/src/index.ts:209-283`, `packages/experimental/hooks-codex/src/index.ts:188-273`) | SessionStart injects context; UserPromptSubmit can reject a step; PreToolUse maps to deny or ask; PostToolUse can block and add context; a blocking Stop hook calls `steer()` to force another step. |
| Auto-review | opt-in experimental | `tools/pre-execute` (`packages/experimental/auto-review/src/index.ts:684`) | An LLM judge returning `ask`-style decisions. |

### Presets and scoped tools

Every agent owns a scope (`agent.ctx`, created at `agent.ts:130`). Tools, prompt sections and listeners registered through a scoped context apply only to that agent and shadow same-named globals. In Web, the `agent-preset-registry` mounts each preset's plugin list once per preset revision into a registry-owned scope, and an agent joins it during setup by binding its scope's parent to that revision's scope key (`packages/preset/agent-preset-registry/src/index.ts:102-121`, `:250-292`; called from `packages/api/session-controller/src/agent.ts:380-395`). The preset therefore decides the agent's tools, its compaction plugin, its plan mode, and whether it runs in PTC mode, without any change to the driver. `tools.restrict()` filters the inherited global tool set for one scope; a filtered tool disappears from the prompt and is refused at execution (`packages/core/tools/src/index.ts:1103-1131`). Subagents use this through `toolFilter` (`packages/subagent/subagent/src/child-agent.ts:216`), and the structured-output child uses a scoped `tools.guard()` to refuse every call after its result is captured (`packages/subagent/subagent/src/structured.ts:119-121`).

## How these change the loop

**Goal mode adds turns, not steps.** When a goal is active and armed, the goal driver listens for `agent/status` becoming `idle` and calls `agent.followup()` with a round prompt whose source is `{kind: 'goal', round}` (`packages/goal/goal-round-driver/src/index.ts:258-285`, `:174-192`). The wake starts a new turn. In that turn's `agent/pre-step`, the driver checks that the reservation still matches the live goal revision; a stale round returns `reject` (turn ends `blocked`) and a valid one returns `{...decision, startsRequestSeries: true}` (`:364-429`), which makes the loop log a fresh `request/header` series. When `roundsStarted` reaches `maxGoalRounds` (default 256) the driver blocks the goal instead of queuing (`:166-171`). A `max-tokens` or aborted turn disarms or pauses the goal (`:332-342`, `:258-285`).

**A blocking Stop hook adds steps inside a turn.** With the experimental Claude Code hooks bridge composed, `agent/turn-stopping` runs the `Stop` hooks; on a deny decision the bridge calls `agent.steer()` (`packages/experimental/hooks-claude-code/src/index.ts:276-283`). The loop re-reads `inbox.nextStep` after the serial dispatch, finds the steering message, and runs another step that claims it (`packages/core/agent-loop/src/agent.ts:361-366`). The bridge carries a `TODO(stop-loop-guard)` noting that consecutive forced continuations are not capped (`hooks-claude-code/src/index.ts:275`).

**Context overflow recovers inside the same step.** The adapter reports `CONTEXT_WINDOW_EXCEEDED` as a terminal failure. The loop appends `assistant/attempt` and dispatches `agent/request-error` (`agent.ts:506-521`). `compaction-basic` sees the code, runs the pruner and then a summarization call that appends a surface `replace`, and returns `{kind: 'retry'}` only if `surface.replaceGeneration` advanced (`packages/compaction/compaction-basic/src/index.ts:190-234`). The loop `continue`s: it re-runs `agent/request` and `prepareCall`, re-projects the system prompt with `startsSeries` true because the surface generation changed (`agent.ts:416-421`), does not re-admit the step's user messages, and derives a new, smaller message list from the log. If compaction does not help, delegation reaches `llm-retry`, which ignores codes outside `retryableCodes`, and the turn ends with `error`. The relative order of the compaction, image-offload and retry listeners follows Cordis registration order (inferred; no explicit `prepend` is set in those three listeners).

**A pre-execute deny becomes a model-visible error, not a stop.** If a `tools/pre-execute` listener (for example a PreToolUse hook) returns `{kind: 'deny', reason}`, the registry skips the body and builds `{isError: true, content: "Error: <reason>"}` (`packages/core/tools/src/index.ts:1525-1538`). That result still goes through `projectContent` and `tools/post-execute`, so `repeat-tool-reminder` still counts it (`packages/guard/repeat-tool-reminder/src/index.ts:191-195`). The scheduler appends it as the call's `tool/result` in model order, and since the step made tool calls the loop runs another step so the model can react (`agent.ts:551-555`). An `ask` instead blocks on `ctx.approval`; with no approval service or no answerer it degrades to a deny with a distinct reason (`index.ts:1733-1776`).

**A tool can end the turn.** `exit_plan_mode` calls `exec.concludeTurn()` when the user dismisses the review (`packages/plan/plan-mode/src/index.ts:337`). The result carries `concludesTurn`, the scheduler ORs it into `concluded` (`tool-calls.ts:158`), and `step()` returns `completed` instead of `null`, so no further request is made unless next-step input (such as same-step `additionalContexts`) is already queued (`agent.ts:555`, `:361-365`).

**Parallel reads, ordered results.** If the model emits `read`, `read`, `bash`, `read`, the first two classify `parallel` (`packages/fs/tool-fs/src/read.ts:136`) and start in one pool; pre-execute for each runs in order before its dispatch. `bash` has no `isConcurrencySafe`, so it is exclusive (`packages/core/tools/src/index.ts:1311`); `fillPool` stops at it, the pool drains, and `bash` runs alone as the next group. Results are appended in model order regardless of finish order (`tool-calls.ts:147-161`).

## Notes for curio-harness

- Keep the driver small and put policy on a short list of typed interception points: pre-step (admission and context), request (route), request-error (recovery by returning `retry`), turn-stopping (continuation by enqueuing input), and the three tool waterfalls. DSH's driver is about 700 lines and almost every product behavior is a listener.
- Make continuation and termination data, not return codes: "another step" means "the next-step inbox is non-empty", and "end the turn" means a result flag. This lets several listeners compose without ordering fights, as the `agent/turn-stopping` doc states (`packages/core/agent/src/runtime-types.ts:366-378`).
- Separate retry from re-admission. DSH retries re-run route selection and request derivation but never re-run pre-step or re-append user input, and every failed attempt is still logged as `assistant/attempt`. Recovery plugins prove progress (surface generation advanced) before asking for a retry.
- Decide on a hard step or forced-continuation cap up front. DSH has none in the loop; runaway control is advisory (repeat reminders) or per feature (goal round cap), and its own hooks bridge has an open TODO for this.
- Order matters in the tool pipeline: log the call first, run policy in model order, overlap only the bodies, commit results in model order with a link to the call, and write a synthetic result for every call that never ran. This keeps the log replayable after abort or crash.
- Classify concurrency fail-closed at the call site (only an explicit `true` is parallel) and re-classify before each start, so a registry change mid-batch cannot run an unsafe call in parallel.

## Corrections to the survey

- §5 Turn step 4 says the goal driver uses `agent/turn-stopping`. It does not. `goal-round-driver` waits for `agent/status` `idle` and queues a new turn with `followup()` (`packages/goal/goal-round-driver/src/index.ts:258-285`, `:192`), then validates it in `agent/pre-step` (`:364-429`). The shipped `agent/turn-stopping` listeners are `dsh-workspace-changes` (which observes and does not steer, `packages/deliverables/workspace-changes/src/index.ts:155`) and the experimental hooks bridges.
- §5 Tool pipeline lists "registered monotonic guards" before "`ctx.approval` for `ask`". The code resolves `ask` through approval first and evaluates guards only when the resulting decision is `allow` (`packages/core/tools/src/index.ts:1511-1525`; `docs/tool-execution-pipeline.md` also says "`ctx.approval` resolves asks before monotonic guards").
- §4 Presets says the registry mounts a preset "under the agent's scoped `agent.ctx`". The registry mounts each preset revision once in its own scope and agents join it by scope-parent binding; several agents can share one mounted generation (`packages/preset/agent-preset-registry/src/index.ts:102-121`, `:250-292`).
- §11 Jobs says completion "arrives as an injected notice". Under the default `completionDelivery: wakeup`, a completion for an idle owner calls `followup()` and opens a new turn; it is injected only when the owner is busy, delivery is `quiet`, or `maxConsecutiveWakes` is exhausted (`packages/jobs/tool-jobs/src/index.ts:295-307`).
- §5 Step 6 says `agent/request-error` runs "on a stream failure". It runs only when the stream ends with an `error` or `aborted` finish chunk (adapter throws are converted to that terminal chunk by `LlmRuntime`). An exception raised while iterating after the stream started is logged as `assistant/attempt` and rethrown without the waterfall (`packages/core/agent-loop/src/agent.ts:462-526`).
- Not stated in the survey: a response that ends with `max-tokens` is committed but its tool calls are not dispatched (`agent.ts:547-550`).
- Minor: the `repeat-tool-reminder` thresholds are at `packages/bundle/base/cordis.patch.yml:446`, not `:444` (the row starts at `:443`).
