# opencode: the agent loop

opencode (V2, commit `8ffbcc2`) runs each Session as a durable, event-sourced loop: prompts are admitted into a SQLite-backed inbox, a per-session execution fiber drains that inbox, and every "step" is exactly one streamed model call built from history reloaded from the database. Tool calls are executed eagerly and in parallel while the model is still streaming, each tool result is recorded as a durable event, and the next step starts only if the model called at least one local tool. Plugins (both built-in and external) shape the loop through mutable hook events and through "transforms" over catalogs of tools, agents, and settings.

Snapshot: [DangerousMindsAi/opencode](https://github.com/DangerousMindsAi/opencode) at commit `8ffbcc2`. All paths are relative to the repository root. "Inferred" marks conclusions that are not read directly from code.

## Pseudo-code

```python
# ---------- 1. Admission (any client: HTTP, TUI, SDK, subagent tool) ----------
def Session.prompt(sessionID, input):                        # delivery defaults to "steer"
    if admission.reconcile(id) exists: return it              # idempotent by message ID
    event = hooks.trigger("session","prompt", {prompt, metadata, delivery})  # hook: session.prompt (mutate)
    item  = materialize files/skills from event                # may change text/files/delivery
    if session.revert: SessionRevert.commit()                  # staged revert committed before new work
    publish InboxEnqueued(item)                                # projector inserts session_inbox row
    SessionExecution.wake(sessionID)                           # advisory doorbell
# compact() admits a "compaction" control item; move() admits a "move" control item; same inbox.

# ---------- 2. Execution coordinator (one busy period per session) ----------
def wake(sessionID, scope="input"):
    if execution active: execution.pendingWake = widest(scope) # coalesce; re-drain before settling
    else: start(): publish Execution.Started + durable claim    # write-ahead claim for crash recovery
         loop: drain(sessionID, force, scope) until no pendingWake
         on exit: publish Execution.Succeeded|Failed|Interrupted (+ release claim unless "shutdown")

# ---------- 3. Drain ----------
def SessionRunner.drain(sessionID, force, continuation=None, promotable="input"):
    if nothing promotable (or only queued items under a steer-only wake): return Complete
    await plugins.awaitActivation
    settleStaleCompactions(); settleStaleToolCalls()           # crash residue -> "interrupted" failures
    step = continuation.step or 1; continuing = bool(continuation)
    while True:
        next = advanceToStep()                                 # may return Complete / Moved / Reloaded
        if next is not Ready: return next                      # Moved/Reloaded -> re-drain at new Location
        continuing = runStep(next.context, step)               # True iff local tools ran this step
        step += 1

def advanceToStep():
    loop:
        if location closed: return Reloaded
        pending = inbox.nextPromotable("input" if entering or not continuing else "steer")
        if pending.type == "move": publish Moved; return Moved
        if pending.type == "compaction":                       # manual /compact, steered ahead of prompts
            compaction.compact(reason="manual")                # hook: session.compaction
            continue
        if not force and not continuing and no eligible pending: return Complete
        selected = context.select(sessionID)                   # agent, permissions, tool snapshot, instructions
        InstructionState.prepare(selected.instructions)        # durable session.instructions.updated deltas
        promoted = inbox.promote(scope)                        # InboxDelivered -> user/synthetic messages
        if promoted > 0:
            step = 1                                           # config: agent.steps allowance resets on input
            maybe fork title generation (top-level, untitled)  # hook: session.title (can supply title)
        return Ready(context.load(selected))                   # resolve model + active history from SQLite

# ---------- 4. Step (logical step policy) ----------
def runStep(loaded, step):
    assistantID = new(); retry = Retry.make()
    recoverOverflow = recoverContinuation = True
    while True:                                                # retries/compactions stay in the same step
        loaded = loaded or prepareContext() + context.load()   # refresh history; never promotes more input
        c = compaction.compact(reason="auto", context=loaded)  # config: compaction.auto/buffer/keep
        if c.failed: raise StepFailed
        if c.completed: assistantID = new(); continue          # rebuild from the new checkpoint
        limit = loaded.agent.steps is set and step >= loaded.agent.steps   # config: agent.steps
        transcript = baseTranscript(agent.system or SystemPrompt(tool names), instructions baseline, history)
        messages = transcript.messages + ([assistant(MAX_STEPS_PROMPT)] if limit else [])
        prepared = context.request.primary(                    # SessionModelRequest.prepare("primary")
            system, messages, tools=loaded.tools,              # tools already filtered by permissions
            toolChoice="none" if limit else None,              # tool defs kept for prompt-cache prefix
            maxTokens=outputLimit(model.limit, inputTokens))
            #   hook: session.context      (mutate system/messages/options/tools; rename/remove tools)
            #   hook: session.model.request (mutate baseURL/headers)
            #   hook: session.http.request / http.response (wrap each HTTP exchange)
            #   hook: session.experimental.ws.{handshake,send,receive} (if config: provider transport=websocket)
        outcome = attempt(prepared, retry=retry.decide, recoverOverflow, recoverContinuation)
        match outcome:
            Completed(needsContinuation): return needsContinuation
            Retry(decision):    retry.wait(decision)           # publish RetryScheduled, sleep; same assistantID
            Continue(decision): retry.wait(); publish Synthetic("continue where you left off"); assistantID = new()
            OutputLimit(nc):    if last 3 user/assistant msgs are all finish=length: raise StepFailed
                                if nc: return True
                                publish Synthetic("hit output limit, break work up"); assistantID = new()
            Compacted:          recoverOverflow = False; assistantID = new()   # one overflow compaction per step
            RecoverFull:        recoverContinuation = False                    # one full-context rebuild

# ---------- 5. Attempt (one physical model call) ----------
def attempt(prepared):
    startSnap = fork(snapshots.capture())                      # config: snapshots (git only)
    for event in llm.stream(prepared.request):                 # own @opencode/ai client
        if overflow error before any output: remember; skip
        publisher.publish(event)                               # Step.Started, text/reasoning, Tool.Called ...
        if event is tool-call and not providerExecuted:        # eager, parallel dispatch
            fork:
                if toolChoice == "none": Tool.Error("Tools are disabled after the maximum agent steps")
                r = prepared.executeTool(call):
                    ev = hooks.trigger("tool","execute.before") # hook: tool.execute.before (rewrite input/name, or fail)
                    reject if tool not advertised in this request
                    tool.execute(decoded input):                # tool calls Permission.assert itself
                        rules = agent.permissions + session.permissions + saved "always" rules
                        effect = last matching rule (default "ask")          # config: permissions, agents.*.permissions
                        hooks.trigger("permission","evaluate")  # hook: permission.evaluate (can change effect)
                        deny -> BlockedError; ask -> wait for user reply
                        decline -> defect tunnel; decline with feedback -> model-visible failure
                    hooks.trigger("tool","execute.after")      # hook: tool.execute.after (rewrite result/error)
                r = toolOutput.truncate(r)                     # config: tool_output.max_lines/max_bytes
                publisher.toolExecution(r) or failTool(err)    # Tool.Success / Tool.Failed
    join all tool fibers; classify declines / interrupts / defects
    if overflow before output and recoverOverflow(): return Compacted   # compaction reason="overflow"
    if transport asks retry-full and no output: return RecoverFull
    if llm error: d = retry.decide(error)                      # hook: session.retry (override decision/delay)
        if d.retry and no output yet: return Retry(d)
    fail unsettled tools; endSnap = snapshots.capture(); files = diff(startSnap, endSnap)
    publish Step.Ended(finish, usage, cost, snapshot, files) or Step.Failed
    if retryable error after output: return Continue(d)
    if user declined a tool: raise Interrupt                   # ends the whole execution, not just the step
    if record.failure (provider error, content filter): raise StepFailed
    nc = toolChoice != "none" and any local tool was called
    return OutputLimit(nc) if finish == "length" else Completed(nc)

# ---------- 6. Interruption ----------
def Session.interrupt(sessionID, resume=False):
    coordinator.interrupt -> Fiber.interrupt(owner)            # streams/tools interrupted; uninterruptible
    # settlement still publishes failed tools, Step.Failed, Execution.Interrupted; user stop cancels jobs
    if resume and next inbox item is steer/control: wake(sessionID, "steer")

# ---------- 7. Crash recovery (server boot) ----------
def SessionRestart.resumeSuspendedSessions():
    for background job (shell, subagent) left pending: deliver synthetic outcome or resume child
    for each top-level session still holding a claim:
        attempts = countResume()                               # durable; default max 10 per turn
        if attempts > max: publish Execution.Failed(RESUME_EXHAUSTED)
        else: publish Synthetic("The server restarted ... continue"); execution.resume(sessionID)
```

### Source map

- Admission: `packages/core/src/session/session.ts:145-178` (prompt, revert commit, wake at `:174`), `packages/core/src/session/prompt.ts:29-90` (`session.prompt` hook at `:40-51`), `packages/core/src/session/inbox.ts:169-202` (admit), `:204-228` (compaction control).
- Coordinator: `packages/core/src/session/execution.ts:81-148` (claim and terminal events), `packages/core/src/session/run-coordinator.ts:74-85` (drain loop with doorbell), `:87-112` (start), `:136-145` (wake coalescing).
- Drain: `packages/core/src/session/runner/llm.ts:57-199`; stale settlement `:332-385`.
- advanceToStep: `packages/core/src/session/runner/llm.ts:74-189`; promotion `packages/core/src/session/inbox.ts:407-424`, `:496-531`, compaction reordering ahead of earlier steers `:533-550`; step reset `llm.ts:109-112,181`; title fork `llm.ts:177-180`.
- Context selection: `packages/core/src/session/context.ts:121-179` (tool snapshot filtered by merged permissions at `:131,134`; instruction ordering `:150-158`).
- Step: `packages/core/src/session/runner/llm.ts:209-330`; auto compaction `:220-225`; max steps `:226-247` and `packages/core/src/session/runner/max-steps.ts:1`.
- Request preparation: `packages/core/src/session/model-request.ts:88-98` (output limit), `:100-120` (base transcript), `:236-420` (prepare), `:253-265` (context hook), `:305-320` (`model.request`), `:337-365` (HTTP hooks), `:368-392` (WebSocket hooks), `:398-399` (retry hook), `:404-418` (executeTool), `:422-431` (hook per request kind).
- Attempt: `packages/core/src/session/runner/step.ts:73-286`; snapshot fork `:76`; eager dispatch `:122-134`; max-steps tool guard `:91-93`; overflow `:155-167`; RecoverFull `:182-188`; retry decision `:189-204`; Step.Ended `:234-261`; Continue `:263-271`; decline interrupt `:274-277`; continuation flag `:281-283`; classification `:298-321`.
- Tool.Called before execution: `packages/core/src/session/runner/publish-llm-event.ts:501-518`; continuation flag `:653-662`; content filter `:587-590`.
- Tool pipeline: `packages/core/src/tool.ts:103-156` (before/after hooks), `:263-284` (dispatch); truncation `packages/core/src/tool-output.ts:67-118`.
- Permissions: `packages/core/src/permission.ts:87-96` (last match wins, default ask), `:158-189` (rule merge and `evaluate` hook), `:231-260` (assert, decline tunnel at `:253`).
- Retry: `packages/core/src/session/runner/retry.ts:32-60` (schedule), `:64-91` (policy and hook), `:94-114` (wait).
- Interruption: `packages/core/src/session/execution.ts:153-170`, `packages/core/src/session/run-coordinator.ts:147-167`.
- Crash recovery: `packages/core/src/session/execution/restart.ts:15-99`, `:192-231`; `packages/core/src/session/store.ts:194-204`.

## What is configurable

| Knob | Where it is set | Default | Where it acts in the loop |
|---|---|---|---|
| Agent `steps` | `agents.<id>.steps` in `opencode.json`, or `steps:` front matter in `agent/*.md` (`packages/schema/src/config/agent.ts:19`, applied at `packages/core/src/config/plugin/agent.ts:120`) | Unset for every built-in agent, so no limit (`packages/core/src/plugin/agent.ts:79-140`) | `runStep`: when `step >= steps`, appends `MAX_STEPS_PROMPT` and sets `toolChoice: "none"` (`packages/core/src/session/runner/llm.ts:226-247`); any tool call is answered with a Tool.Error (`step.ts:91-93`) and no continuation follows (`step.ts:281`). |
| Agent `system` | `agents.<id>.system` or the Markdown body of an agent file (`packages/core/src/config/plugin/agent.ts:115,203`) | Built-in base prompt | Replaces the base system prompt in `baseTranscript` (`packages/core/src/session/model-request.ts:110-114`) and suppresses per-model prompt overrides (`packages/core/src/plugin/optimize.ts:81`). |
| Agent and session `permissions`; top-level `permissions` | `agents.<id>.permissions`, top-level `permissions` (`packages/schema/src/config.ts:55-57`), session API `setPermissions` (`packages/core/src/session/session.ts:82-88`) | Allow `*`, ask for `*.env` reads (`packages/schema/src/agent.ts:46-51`) | Wholly denied tools are removed from the per-step tool snapshot (`packages/core/src/tool.ts:230-233,292-295`); per-call checks in `Permission.assert` (`packages/core/src/permission.ts:158-189`). Top-level rules are appended to every agent's list (`packages/core/src/config/plugin/agent.ts:84-92`). |
| Default agent | `default_agent` (`packages/schema/src/config.ts:36`) | `build` (`packages/core/src/agent.ts:13`) | Selects the agent in `context.select` (`packages/core/src/session/context.ts:128`). |
| Model and variant | Top-level `model` (`packages/schema/src/config.ts:33`), session `switchModel`, `provider/model#variant` syntax (`packages/schema/src/config/model.ts:18-36`) | Resolver default when the session has none (`packages/core/src/session/runner/model.ts:77-81`) | `context.load` resolves the session model each step (`packages/core/src/session/context.ts:163-164`). An agent's `model` is applied only when a session is created or switched by the subagent tool or a command (`packages/core/src/tool/plugin/subagent.ts:173,184`; `packages/core/src/config/plugin/command.ts:105`), and for the compaction and title agents. |
| Provider and model request overlays (`settings`, `headers`, `body`) | `providers.<id>` and per-model entries (`packages/schema/src/config/provider.ts:10-49`) | None | Folded into the model definition (`packages/core/src/models-dev.ts:247-248`), so they apply to every request. |
| Agent `request` overlay | `agents.<id>.request` (`packages/schema/src/config/agent.ts:13`) | Empty | Stored on the agent (`packages/core/src/config/plugin/agent.ts:111-114`). A repository-wide search found no code that applies it to model requests (inferred: currently inert). |
| Provider `transport`, `timeout`, `chunkTimeout` | `providers.<id>.settings` (`packages/schema/src/provider.ts:52-60`) | HTTP | `transport: "websocket"` binds a session WebSocket and enables the `experimental.ws.*` hooks (`packages/core/src/session/model-request.ts:368-392`). |
| Provider `compaction` type | `providers.<id>.settings.compaction` or model settings (`packages/schema/src/provider.ts:37-42`) | `summary` | `native` sends the provider's own compaction instead of a summary request (`packages/core/src/session/compaction.ts:213,231-233`). |
| `compaction.auto` | `compaction.auto` (`packages/schema/src/config/compaction.ts:11`) | `true` (`packages/core/src/session/compaction.ts:197`) | When false, both auto and overflow compaction are skipped; only manual `/compact` runs (`compaction.ts:210`). |
| `compaction.buffer` | `compaction.buffer` | Unset: keep 10% of the window free, or 16k when the window is at least 32k (`packages/core/src/session/compaction.ts:905-912`) | Ceiling compared with the prompt estimate before each step (`compaction.ts:211,242-251`). |
| `compaction.keep.tokens` | `compaction.keep.tokens` | `15_000` (`packages/core/src/session/compaction.ts:197`) | Verbatim recent tail kept beside the summary (`compaction.ts:278`). |
| Tool output limits | `tool_output.max_lines`, `tool_output.max_bytes` (`packages/schema/src/config/tool-output.ts:6-9`; applied at `packages/core/src/config/plugin/tool-output.ts:15-22`) | 2,000 lines, 50 KiB (`packages/core/src/tool-output.ts:13-14`) | `toolOutput.truncate` on every local tool result (`packages/core/src/session/runner/step.ts:127`). |
| Snapshots | `snapshots` (`packages/schema/src/config.ts:64-66`; applied at `packages/core/src/config/plugin/snapshot.ts:15-19`) | `true`, effective only when the Location's VCS is git (`packages/core/src/snapshot.ts:82,114`) | Start and end capture around each attempt and the `files` list on `Step.Ended` (`step.ts:76,236-245`). |
| Code Mode | Per MCP server `mcp.servers.<name>.codemode` (`packages/schema/src/mcp.ts:34,59`); per tool `options.codemode`; denying permission action `execute` | MCP and `opencode_*` tools go through Code Mode; built-ins set `codemode: false` (`packages/core/src/tool/mcp.ts:45`; for example `packages/core/src/tool/plugin/read.ts:42`) | Tool snapshot splits direct tools from the single `execute` tool and builds the catalog instruction (`packages/core/src/tool.ts:234-261`; `packages/core/src/session/context.ts:151`). |
| Delivery mode | `delivery: "steer" \| "queue"` on prompt, synthetic, and compact calls; can be changed while pending (`packages/core/src/session/session.ts:134-144`) | `steer` (`packages/core/src/session/prompt.ts:50`) | Steers are promoted at the next step boundary; queued items only at an idle boundary (`packages/core/src/session/inbox.ts:407-424,496-531`). |
| Subagent depth | `experimental.subagent_depth` (`packages/schema/src/config/experimental.ts:11-13`) | 1 | The `subagent` tool refuses deeper nesting (`packages/core/src/tool/plugin/subagent.ts:129-133`). |
| Warming | `warming` (`packages/schema/src/config.ts:109-111`, `packages/schema/src/config/warming.ts`) | Off | A `context` hook schedules keep-alive requests after primary requests (`packages/core/src/plugin/warming.ts:103-105`). |
| Instructions | `instructions` paths/URLs, `skills`, `references`, `mcp` (`packages/schema/src/config.ts:82-99`) | AGENTS.md discovery and built-ins | Loaded each step in `context.select` and admitted as durable deltas (`packages/core/src/session/context.ts:132-158`; `packages/core/src/session/runner/llm.ts:201-206`). |
| Plugins | `plugins` enablement list (`packages/schema/src/config.ts:103-105`) | All internal plugins on | Removes or adds hook and transform registrations; policy plugins cannot be removed (`packages/core/src/plugin/internal.ts:274`). |
| Retry policy | Not in config. Only the `session.retry` hook | 10 retries, 2 s exponential capped at 10 s per gap, jittered, `retry-after` honoured up to 15 min, at most 3 timeout retries (`packages/core/src/session/runner/retry.ts:32-60`) | `attempt` asks `retry.decide` on LLM errors (`step.ts:189-199`). |
| Crash-resume budget | `SessionRestart.layer({ maxAttempts })` for embedders, not config | 10 (`packages/core/src/session/execution/restart.ts:33`) | Boot-time sweep (`restart.ts:77-99`). |
| Output token limit | Not config; hooks may change `options.maxTokens` | Catalog output limit, capped at 256k, fitted to the remaining window (`packages/core/src/session/model-request.ts:49-57,88-98`) | Set on the draft before the `context` hook (`model-request.ts:259-262`). |

## Plugins and hooks

**What a plugin is.** A plugin is `{ id, effect(ctx) }`, where `ctx` exposes domains such as `agent`, `tool`, `session`, `permission`, `shell`, `aisdk`, `model`, `provider`, `skill`, `mcp`, and `event` (`packages/plugin/src/effect/plugin.ts:26-64`). There are two extension mechanisms:

- **Transforms** are synchronous editors over Location-scoped catalogs (tools, agents, commands, skills, MCP, models, providers, and service settings such as compaction or tool output). Registrations are scoped and replayed on reload (`packages/plugin/src/effect/registration.ts:27`; `packages/core/src/tool.ts:159-219`).
- **Hooks** receive a mutable event object. `PluginHooks.trigger` runs every registered callback in registration order on the same object and returns it, so later callbacks see earlier edits (`packages/core/src/plugin/hooks.ts:88-95`). A hook may be scoped to one provider with `{ providerID }` (`packages/plugin/src/effect/registration.ts:7-25`; `hooks.ts:90`). Only `tool.execute.before` has a typed failure channel (`hooks.ts:23-30`).

Activation order is internal `pre` plugins, then SDK and instance plugins, then external packages, then internal `post` (config loader) plugins (`packages/core/src/plugin/supervisor.ts:99-103,152-161`; `packages/core/src/plugin/internal.ts:216-270`). Because hooks run in registration order, an external plugin's `context` hook runs after the built-in prompt and tool hooks (inferred from that ordering). The drain waits for activation before running (`packages/core/src/session/runner/llm.ts:70`).

**Hooks that touch the loop** (types in `packages/plugin/src/effect/session.ts:138-151`, `tool.ts:20-52`, `permission.ts:10-23`, `shell.ts:3-13`):

| Hook | Fires at (pseudo-code) | What it can do |
|---|---|---|
| `session.prompt` | Admission, before attachments are materialized (`packages/core/src/session/prompt.ts:40-51`) | Mutate prompt text, files, agents, skills, metadata, and `delivery`. |
| `session.context` | Step, inside `context.request.primary` (`packages/core/src/session/model-request.ts:253-265,427`) | Mutate `system`, `messages`, `options` (generation settings and provider options, including `maxTokens`), and `tools` (edit descriptions or schemas, rename by moving a definition, delete). Tools a hook invents are dropped (`model-request.ts:266-274`). |
| `session.compaction` | Compaction request preparation (`packages/core/src/session/compaction.ts:575-592`) | Same as `context`, plus set `result` to supply the summary and skip the model call (`compaction.ts:396`; `packages/plugin/src/effect/session.ts:45-48`). |
| `session.generate`, `session.title` | Auxiliary requests (`model-request.ts:428-430`) | `title` can set `result` to skip the call (`packages/plugin/src/effect/session.ts:52-55`). |
| `session.model.request` | After the context hook, every request kind (`model-request.ts:305-320`) | Change `baseURL` and headers. |
| `session.http.request` / `http.response` | Around every HTTP exchange of the request (`model-request.ts:337-365`) | Replace the outgoing `Request` or the incoming `Response`. |
| `session.experimental.ws.handshake/send/receive` | WebSocket transport only (`model-request.ts:368-392`) | Change URL and headers, or rewrite frames. |
| `session.retry` | Attempt classification on an LLM error, and inside compaction retries (`packages/core/src/session/runner/retry.ts:78-91`; `compaction.ts:444-451`) | Override `decision` to stop retrying or change the delay. |
| `tool.execute.before` | Before input decoding, for direct and Code Mode inner calls (`packages/core/src/tool.ts:103-111,240-243,271`) | Rewrite `input` or the tool name, or fail with `Tool.Error` to reject the call. |
| `tool.execute.after` | After the tool returns or fails (`tool.ts:131-155`) | Replace the result, or replace the error. |
| `permission.evaluate` | Inside `Permission.assert`/`ask`, after configured `deny` rules (`packages/core/src/permission.ts:173-189`) | Change `effect` (allow, ask, deny) and `message`. A configured deny short-circuits before the hook runs (`permission.ts:175`). |
| `shell.create.before` | When the shell service spawns a command (`packages/core/src/shell.ts:274`) | Change command, cwd, timeout, shell, and env. |
| `aisdk.sdk` / `aisdk.language` | When an AI SDK compatibility provider is constructed (`packages/plugin/src/effect/aisdk.ts:5-17`) | Supply or replace the SDK or language model. |
| Event bus subscription | Any time (`ctx.event.subscribe`) | Observe only; a plugin can act by calling `ctx.session.*` (for example `synthetic`, `prompt`, `interrupt`). |

**Built-in plugins that implement loop behavior.**

- `PlanPlugin` (`packages/core/src/plugin/plan.ts:27-97`) edits the `plan` agent's permissions, rewrites blocked edit errors in `tool.execute.after`, and uses a `session.context` hook to insert enter or leave `<system-reminder>` messages near the tail, persisting each one as a synthetic message so it stays reconstructable.
- `OptimizePlugin.Plugins` (`packages/core/src/plugin/optimize.ts:16-93`) replace or append the system prompt per model family in `context`, `compaction`, and `generate`. Two tool-pruning variants exist but are not in the exported list (`optimize.ts:32-49,62`).
- `PatchTool` registers `patch` and its `context` hook removes `edit`/`write` or `patch` depending on the model (`packages/core/src/tool/plugin/patch.ts:296-309`). `ShellTool` rewrites its own description per detected shell (`packages/core/src/tool/plugin/shell.ts:283-291`).
- `IdentityPlugin` and `VerbosityPlugin` hook `context` and related kinds (`packages/core/src/plugin/identity.ts:29-31`; `packages/core/src/plugin/verbosity.ts:38-41`). Provider plugins use `model.request`, `http.request`, and `context` (for example the OpenAI plugin removes `maxTokens`, `packages/core/src/plugin/provider/openai.ts:308-331`).
- `ToolInputRepairPlugin` repairs model JSON in `tool.execute.before` (`packages/core/src/plugin/tool-input-repair.ts:21-36`).
- `ConfigPolicyPlugin` enforces organization `tool.use` denies through `permission.evaluate` and is guarded against removal (`packages/core/src/config/plugin/policy.ts:48-62`; `packages/core/src/plugin/internal.ts:274`).
- `ConfigCompactionPlugin`, `ConfigToolOutputPlugin`, `ConfigSnapshotPlugin`, and `ConfigAgentPlugin` are transforms that move config values into the corresponding services (`packages/core/src/config/plugin/compaction.ts:15-23`, `tool-output.ts:15-22`, `snapshot.ts:15-19`, `agent.ts:83-127`). Compaction, snapshots, and truncation themselves are core services called directly by the runner, not plugins.
- `WarmingPlugin` hooks `context` to start keep-alive loops (`packages/core/src/plugin/warming.ts:103-105`).

**Agents, subagents, MCP, and Code Mode.**

- Agents change the loop through `system`, `permissions` (which filter the tool snapshot and drive per-call checks), and `steps`. Built-ins: `build` (adds `question: allow`), `plan`, `general` (denies `question` and `subagent`), `explore` (deny-all then allows read, grep, glob, shell, webfetch, websearch, external_directory), and hidden `compaction`, `title`, `summary` (`packages/core/src/plugin/agent.ts:79-140`).
- A subagent is a child Session. The `subagent` tool admits a prompt into the child's inbox and either blocks on the child's job (foreground) or returns immediately and later delivers a synthetic completion message to the parent (`packages/core/src/tool/plugin/subagent.ts:205-256`). The child runs the same loop with its own agent, model, and step counter.
- MCP tools enter the tool registry like any other tool. With Code Mode they are not advertised as native tools; the model sees one `execute` tool plus a catalog instruction, and each inner call still passes `tool.execute.before/after` (`packages/core/src/tool.ts:234-261`, `:240-243`). MCP server instructions are loaded as instruction sources each step (`packages/core/src/session/context.ts:139`).

## How these change the loop

**The plan agent.** When a session uses `plan`, `context.select` merges its permissions, which end with `edit: deny` for `*` and `edit: allow` for `~/.opencode/plan/*` (`packages/core/src/plugin/plan.ts:33-41`). The edit tools stay advertised: `whollyDisabled` removes a tool only when the last rule matching its action is a deny on resource `*`, and here the last `edit` rule is the plan-directory allow (`packages/core/src/tool.ts:292-295`). Each edit call outside the plan directory fails in `Permission.assert` with `BlockedError`, and the plan plugin's `execute.after` hook rewrites the error into a clear model-visible message (`plan.ts:44-53`). The `context` hook inserts the Plan reminder just before the latest user message and persists it (`plan.ts:57-73`). Inferred: because top-level `permissions` are appended to every agent after the built-in rules (`packages/core/src/config/plugin/agent.ts:84-92`) and the last match wins, a user's top-level `edit: allow` rule would override the plan agent's deny.

**Hitting the step limit.** With `agents.build.steps: 5`, steps 1 to 4 run normally. On step 5, `runStep` appends `MAX_STEPS_PROMPT` as an assistant message and sets `toolChoice: "none"` while keeping tool definitions (`packages/core/src/session/runner/llm.ts:226-247`). If the model still emits a tool call, the attempt answers it with "Tools are disabled after the maximum agent steps" (`step.ts:91-93`), and `needsContinuation` is forced false (`step.ts:281`), so the drain ends. The allowance resets whenever input is promoted, including a steer that arrives mid-turn (`llm.ts:181`). Retries, continuations, and compactions inside one step do not consume the allowance (`llm.ts:216-329`).

**Context overflow.** If the provider rejects the request as too long before any output, the attempt calls `recoverOverflow`, which runs `compaction.compact({ reason: "overflow" })` aiming at 70% of the current estimate (`packages/core/src/session/compaction.ts:227-228`), and returns `Compacted`. The step rebuilds from the new checkpoint with a fresh assistant message ID. A second overflow in the same step is not compacted again and surfaces as an error (`llm.ts:265-271,320-323`).

**A plugin that blocks dangerous shell commands.** Register `ctx.permission.hook("evaluate", e => { if (e.action === "shell" && e.resources.some(r => r.includes("rm -rf"))) { e.effect = "deny"; e.message = "blocked by policy" } })`. The shell tool's own `Permission.assert` then fails with `BlockedError`, which the tool reports as a model-visible failure, and the loop continues (`packages/core/src/permission.ts:173-189,237-244`). Alternatively, `tool.execute.before` can fail with `Tool.Error` to reject the call before the tool runs (`packages/core/src/plugin/hooks.ts:23`).

**A plugin that supplies its own compaction.** Register `ctx.session.hook("compaction", e => { e.result = { summary: mySummary(e.messages) } })`. `deliver` sees `prepared.event.result` and records that summary without a model call (`packages/core/src/session/compaction.ts:396,595-610`).

**Steering versus queueing.** A prompt sent with `delivery: "steer"` while the model is working is promoted at the next step boundary, so the next model call sees it after the current tool results (`packages/core/src/session/runner/llm.ts:86-90`; `packages/core/src/session/inbox.ts:496-510`). A `queue` prompt waits until the step returns `needsContinuation = false`, then starts a fresh turn within the same busy period (`llm.ts:164-165`; `inbox.ts:512-528`).

**A plain decline.** When the user rejects a permission prompt without feedback, the decline travels as a defect past tool error handling, is recovered in `executeTool`, and the attempt ends the execution with `Effect.interrupt` after marking the call "The user declined this tool call" (`packages/core/src/permission.ts:247-253`; `packages/core/src/session/model-request.ts:404-418`; `packages/core/src/session/runner/step.ts:207-215,274-277`). A decline with feedback (`CorrectedError`) stays a normal tool failure and the loop continues.

## Notes for curio-harness

- Separate admission from execution. A durable inbox with `steer` and `queue` delivery, plus a per-session coordinator with a coalescing doorbell (`packages/core/src/session/run-coordinator.ts:74-145`), gives mid-turn steering, idle queueing, and manual compaction as one mechanism.
- Keep logical step policy (compaction, step limit, retry, continuation) in one function and the physical model call in another that returns a closed outcome union (`packages/core/src/session/runner/step.ts:32-43`). This makes retry, output-limit continuation, overflow compaction, and full-context rebuild explicit and testable.
- Publish `Tool.Called` durably before side effects and settle any `running` tool call at drain start (`packages/core/src/session/runner/publish-llm-event.ts:510-517`; `llm.ts:362-385`). With a write-ahead execution claim, crash recovery becomes a boot-time sweep rather than shutdown logic.
- Make hooks plain mutable events with ordered callbacks and a narrow failure channel. opencode exposes request shaping (`context`), transport (`model.request`, `http.*`), retry, tool before/after, and permission evaluation, which covers most extension needs without loop changes.
- Keep tool definitions stable across the final step (`toolChoice: "none"` instead of removing tools) and append instruction changes as deltas, so prompt-cache prefixes survive.
- Avoid configuration that parses but has no effect. The agent `request` overlay appears to be inert, and the step allowance resets on any promoted steer; both are easy to miss without an explicit test.

## Corrections to the survey

- §6 says tool output truncation is a "head-plus-tail cut". The code keeps only the first lines that fit both limits and appends a marker with the spill path (`packages/core/src/tool-output.ts:78-112`).
- §9 says Explore "denies everything except read-only tools". Explore also allows `shell` and `external_directory`; read-only shell use is requested only by its prompt (`packages/core/src/plugin/agent.ts:7-17,104-117`).
- §10 says each custom agent has its own "request overlays". The `request` field is parsed and stored (`packages/core/src/config/plugin/agent.ts:111-114`), but no code applying it to model requests was found. Similarly, an agent's `model` is applied when a session is created or switched by subagents and commands, not on every step.
- §4 says `context.request.primary` "selects tools". Tool selection happens earlier in `context.select` via `registry.snapshot(permissions)` (`packages/core/src/session/context.ts:131-134`); the `context` hook can only edit, rename, or remove the selected tools.
- §4 says "new user input resets the allowance". Any promoted input resets it, including steers delivered mid-turn (`packages/core/src/session/runner/llm.ts:181`).
- §9 says "a plain decline interrupts the step". It interrupts the whole execution (`packages/core/src/session/runner/step.ts:274-277`), and a runner test expects an interrupt-only exit. Inferred and not covered by a test I found: because no interruption reason is recorded for this self-interrupt, `terminal()` classifies it as `shutdown`, which keeps the execution claim and skips job cancellation (`packages/core/src/session/execution.ts:50-53,128-135`).
- §4 crash recovery: the budget is 10 resumes per turn by default (`packages/core/src/session/execution/restart.ts:33`), and running background subagent children are also resumed through their jobs, not only top-level sessions (`restart.ts:138-190,196-205`).
