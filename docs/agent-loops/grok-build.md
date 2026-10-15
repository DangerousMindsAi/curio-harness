# Grok Build: the agent loop

Grok Build runs one `SessionActor` per ACP session; its `run_session` task is a `tokio::select!` loop over commands, timers and turn-completion messages, and it spawns at most one turn task at a time from a prompt queue. A turn (`handle_turn_input_inner`) runs an outer "round" loop that repeats while goal mode or a `Stop` hook asks for more work, and each round runs an inner sampling loop (`process_conversation_turn_inner`) that samples the model, dispatches tool calls, and repeats until the model answers without tool calls or a stop condition fires.

Snapshot: [DangerousMindsAi/grok-build](https://github.com/DangerousMindsAi/grok-build) at commit `2bdd1d6`. All paths are relative to the repository root. "Inferred" marks a claim not read directly from code.

## Pseudo-code

The loop has three levels. A **turn** answers one prompt. A turn runs one or more **rounds**: a round ends when the model stops calling tools, and a new round starts only if goal mode or a Stop hook wants more work. A round is a series of **sampling steps**; each step is one model call plus the tools it asks for. Lines marked `[hook]` are where user hooks can intervene, `[reminder]` marks a note the harness adds to the conversation, and `[config]` marks a setting; the tables below give the details.

```text
# ---------- The session ----------
each session has one actor that handles commands one at a time:
    a new prompt     -> put it in the queue; if nothing is running, start a turn
                        ("send now" cancels the running turn first)
    cancel           -> stop the running turn
    turn finished    -> in goal mode, queue the next goal round; then start the next queued prompt
    session closed   -> run SessionEnd hooks and stop                [hook] SessionEnd

# ---------- A turn ----------
run_turn(prompt):
    if it is a built-in slash command, run it and stop
    let hooks see the prompt and possibly block it               [hook] UserPromptSubmit
    expand skills and images; if the prompt is huge, save it to a file
        and give the model an excerpt
    add the prompt to the conversation

    loop:                                                        # rounds
        result = run_round()
        if the round did not finish normally (cancelled, step limit, stuck): stop
        if goal mode is on and no other prompt is waiting:
            ask the goal evaluator whether the goal is done
            if not done: add its next instruction and run another round   [reminder]
            if it looks done: a panel of skeptic subagents must agree first
        ask the Stop hooks whether the agent may stop               [hook] Stop
            if a hook says "keep working": add its feedback and run another round
            (at most 8 times per turn)
        stop

    save a rewind point so files and conversation can be rolled back
    record usage and report how the turn ended to the client

# ---------- A round ----------
run_round():
    loop:                                                         # sampling steps
        if the model has made the same tool calls over and over:
            after 4-8 repeats add a "you are repeating yourself" note       [reminder]
            after 8-12 repeats end the turn
        add anything that arrived in the meantime: messages the user typed
            mid-turn, skill reminders, output from watched scripts
        if the context is nearly full: summarize older history      [config] auto_compact_threshold_percent
                                                                    [hook] PreCompact, PostCompact
        call the model and stream the reply to the client
            on network errors, rate limits or server errors: wait and retry   [config] max_retries (15)
            if the server flags the reply as looping: retry with a warning
            if the context is too long: summarize and retry
            if the login expired: refresh it and retry

        if the reply was cut off by the output limit:
            with tool calls: add a reminder and carry on (the turn fails after 5 in a row)
            without tool calls: ask it to continue, if allowed         [config] length salvage budget (off)

        if the reply has no tool calls:
            if todos are unfinished: remind the model and continue     [config] TodoGate (off)
            if the user typed something mid-turn: continue with it
            the round is done

        run_tools(tool calls)
        if a tool call was rejected by the user: end the turn
        if the number of tool steps reached the limit: end the turn   [config] --max-turns (unlimited)

# ---------- Running tools ----------
run_tools(calls):
    first check each call, one at a time, in order:
        let hooks deny it, ask the user, or rewrite its arguments    [hook] PreToolUse
        in plan mode, refuse edits to anything except the plan file
        check permissions: rules, earlier approvals, and the mode    [config] permission mode
            in auto mode, a quick check and then a language model decide
        if denied by a rule or hook: the model sees "not executed" and can adapt
        if the user rejects it: skip the remaining calls and end the turn

    then run all approved calls at the same time,
        except that calls writing to the same file take turns
    after each call, let hooks add context or replace the result   [hook] PostToolUse
    add the results to the conversation
```

### Where each part lives

- The session actor: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/run_loop.rs:354` (command loop `:522`, new prompt `:725`, cancel `:1143`, turn finished `:2294`, `SessionEnd` `:78`); prompt queue `prompt_queue.rs:113`.
- A turn: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs:503` (`UserPromptSubmit` `:911`, skills and images `:985-1030`, large-prompt offload `:1075` and `prompt_offload.rs`); the round loop with goal and Stop hooks `turn.rs:1308-1398`; Stop gate `stop_gate.rs:9,236-336`; goal evaluation `goal.rs:1400-1492`; skeptic panel `goal.rs:164-225,441-620`; rewind point and end of turn `turn.rs:1399-1420,1688-1716`.
- A round: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs:2615` (loop `:2747`, repeated-call check `:2750-2828` with thresholds `:3896-3900`, mid-turn messages `:2829-2833`, compaction `:2853-2880`, model call `:3051`, retry handling `:3062-3290`, output-limit handling `:3488,3578`, TodoGate `:3624-3678`, step limit `:3851-3862`); retries in the sampler `crates/codegen/xai-grok-sampler/src/retry.rs:5-50`, looping detection `crates/codegen/xai-grok-sampler/src/doom_loop_recovery.rs:1-29`, login refresh `auth_retry.rs:111-117`.
- Running tools: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs:335-400` (checking each call `:484-528,1445`, `PreToolUse` `:1301-1385`, plan mode `:1714-1729`, permissions `:1849-1984`, same-file locks `:622-650`, running in parallel `:1008`, `PostToolUse` `:1168-1205`).

All files named without a directory in this list are in `crates/codegen/xai-grok-shell/src/session/acp_session_impl/`.

## What is configurable

Only knobs that change loop behavior are listed. "Remote" means a field of the signed `/v1/settings` payload (`crates/codegen/xai-grok-config/src/remote_settings.rs`).

| Knob | Where it is set | Default | Effect in the loop |
|---|---|---|---|
| Max tool rounds | CLI `--max-turns N` (`crates/codegen/xai-grok-pager/src/app/cli.rs:666-670`); agent definition `maxTurns` (`crates/codegen/xai-grok-agent/src/config.rs:740`); CLI wins (`crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs:4550-4556`); subagents use their own `maxTurns` or inherit the parent's (`crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs:1718-1725`) | unlimited | After each tool round, `tool_turn_count + 1 > limit` returns `MaxTurnsReached` (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs:3851-3862`); ACP sees `StopReason::Cancelled` (`turn.rs:1712-1716`). |
| Length salvage budget | env `GROK_LENGTH_SALVAGE` (true gives 2, false kills); remote `length_salvage_budget` (`remote_settings.rs:450`, 0 kills); always 5 for the Cursor agent (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/length_salvage.rs:6-45`) | off | A text answer cut at `Length` gets a continue reminder and is resampled up to the budget (`turn.rs:3578-3619`). |
| Length tool-call streak cap | fixed `MAX_OUTPUT_TOKEN_LIMIT_RETRIES = 5` (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:17`) | 5 | Consecutive truncated tool-call samples beyond 5 fail the turn (`turn.rs:3488-3505`). |
| TodoGate | hidden CLI `--todo-gate` (`cli.rs:726-728`); remote `todo_gate_enabled`, `todo_gate_max_fires_per_prompt` (`remote_settings.rs:438,443`) | off; 2 fires (`crates/codegen/xai-grok-agent/src/system_reminder.rs:6,62-68`) | When the model stops with pending todos, a reminder is pushed and the loop continues; disabled while a goal is active (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/reminders.rs:491-505`, `turn.rs:3624-3678`). |
| Action stationarity | fixed constants (`turn.rs:3896-3900`) | nudge 8 / 4, stop 12 / 8, `true` stop 4 | Not configurable. |
| Stop-hook continuation cap | fixed `MAX_STOP_HOOK_CONTINUATIONS_PER_TURN` (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/stop_gate.rs:9`) | 8 | After 8 `KeepWorking` verdicts the gate allows the stop without consulting hooks (`stop_gate.rs:250-260`). |
| Structured output | per-prompt `json_schema` on the ACP prompt (`run_loop.rs:725`); fixed retry cap `STRUCTURED_OUTPUT_MAX_RETRIES = 3` (`turn.rs:18`) | none | Native schema on backends that support it, otherwise a synthetic `StructuredOutput` tool plus reminder (`turn.rs:2719-2746,2947-2957`). |
| Completion requirement | agent definition `completionRequirement {tool, reminder, recovery{maxRetries, baseDelayMs, maxDelayMs}}` (`crates/codegen/xai-grok-agent/src/config.rs:760,790-809`) | none | Re-runs the inner loop with a recovery reminder until the named tool is called (`turn.rs:1977-2126`). |
| Sampler per-request retries | env `GROK_MAX_RETRIES`, else model/config `max_retries` (`crates/codegen/xai-grok-sampler/src/retry.rs:15-28`) | 15 (`retry.rs:9`); backoff 2 s doubling to 30 s with jitter (`retry.rs:43-50`) | Retries inside one `run_turn_via_sampler` call. |
| Turn-level transient resubmit | `[features] turn_transient_retry`, env `GROK_TURN_TRANSIENT_RETRY`, remote `turn_transient_retry` (`crates/codegen/xai-grok-shell/src/util/config/resolve/features.rs:25-40`, `remote_settings.rs:297`) | on for main sessions, off for subagents (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs:1806-1811`) | Stalls, transport errors and retryable 5xx are resubmitted: 3 per step, 10 per prompt, within max(10 min, 2 x idle timeout) (`sampler_turn.rs:19-86`). |
| Inference idle timeout | model config `inference_idle_timeout_secs` (`crates/codegen/xai-grok-shell/src/agent/config.rs:545`); remote (`remote_settings.rs:408`) | not traced | Stream stall detection; also widens the transient retry window. |
| Uncharged 401 park | remote `uncharged_401_park` (`remote_settings.rs:301`); `GROK_UNCHARGED_401_PARK=0` (`crates/codegen/xai-grok-shell/src/util/config/resolve/flags.rs:3-17`) | on | Lets a turn wait for a token after a credential-less 401; auth retries capped at 3, uncharged resubmits at 50 (`auth_retry.rs:111-114`). |
| Doom-loop recovery | env `GROK_DOOM_LOOP_RECOVERY`; TOML `[doom_loop_recovery]`; remote `doom_loop_recovery` (`crates/codegen/xai-grok-shell/src/agent/config.rs:1960-1997`, `remote_settings.rs:21-36,426`) | on; `max_threshold` 64, `max_retries` 2, `window_tokens` 1024 (`crates/codegen/xai-grok-sampling-types/src/doom_loop.rs:79-81`) | Sampler aborts a flagged stream and resamples with a looping reminder. |
| Auto-compact threshold | env `GROK_AUTO_COMPACT_THRESHOLD_PERCENT`; TOML `[model.<id>]` and `[session]` `auto_compact_threshold_percent`; remote per-model and global (`crates/codegen/xai-grok-shell/src/util/config/resolve/compaction.rs:2,36-53`, `remote_settings.rs:861`) | 85; 80 for the bundled models (`crates/codegen/xai-grok-models/default_models.json:19,57`) | Pre-sampling `check_auto_compact_needed` (`crates/codegen/xai-grok-shell/src/session/compaction.rs:2151`). |
| Two-pass prefire | `features.two_pass_compaction`, env `GROK_TWO_PASS_COMPACTION`, remote `two_pass_compaction_enabled` (`crates/codegen/xai-grok-config-types/src/registry.rs:256-263`); lead env `GROK_PREFIRE_LEAD_PERCENT` (`compaction.rs:63`) | on; lead 10 points | Starts pass-1 summarization in the background once usage reaches threshold minus lead (`compaction.rs:180-193`). |
| Compaction wall clock | remote `compaction_wall_clock_budget_secs` (`remote_settings.rs:886`) | 300 s (`crates/codegen/xai-grok-agent/src/compaction.rs:29-35`) | Bounds each compaction call. |
| Long-reasoning reminder | env `GROK_LONG_REASONING_REMINDER`; TOML `[long_reasoning_reminder]`; remote (`crates/codegen/xai-grok-shell/src/session/long_reasoning_reminder.rs:9-15,43-67`, `remote_settings.rs:755`) | off; 1000 tokens; delay 1 call | Pushes a "stop reasoning at length" reminder after a long reasoning step (`turn.rs:2881-2901`). |
| Goal mode | env `GROK_GOAL`, TOML `[goal] enabled`, remote `goal_enabled` (remote `false` is a kill switch) (`crates/codegen/xai-grok-shell/src/agent/config.rs:2160-2170`) | on | Enables `/goal` and the round-end evaluator. |
| Goal verifier count, classifier cap, strategist cadence | env `GROK_GOAL_VERIFIER_N`, `GROK_GOAL_CLASSIFIER_MAX`, `GROK_GOAL_STRATEGIST_EVERY`; TOML `[goal]`; remote (`agent/config.rs:2245-2287`, `remote_settings.rs:502-510`) | 3 skeptics (1 to 5), 10 runs, every max(1, cap/2) (`crates/codegen/xai-grok-shell/src/session/goal_classifier.rs:34,78-83`) | Size of the skeptic panel and when the strategist fires. |
| Workflows | env `GROK_WORKFLOWS`, TOML `[workflows] enabled`, remote `workflows_enabled` (`agent/config.rs:2171-2186`) | on | Enables the `workflow` tool and moves goal round-end onto the workflow engine (`crates/codegen/xai-grok-shell/src/session/acp_session.rs:1345-1347`). |
| Permission mode | CLI `--permission-mode` (`cli.rs:673`), `--always-approve`/`--yolo` (`cli.rs:275`); remote `permission_mode` (`remote_settings.rs:815`); runtime `SetAutoMode`/`SetYoloMode` | `default` | Selects the prompt policy in the permission manager. |
| Auto mode classifier | TOML `[auto_mode]`: `enabled`, `prompt_type`, `classifier_model`, `classify_timeout_ms`, `reasoning_effort` (`agent/config.rs:4127-4147`); remote `auto_mode` (`remote_settings.rs:800`) | prompt type `just_command` | Shapes the LLM side query; denial limits 3 consecutive / 20 total are fixed (`crates/codegen/xai-grok-workspace/src/permission/manager/request_classification.rs:7-8`). |

## Plugins, hooks and extensions

**Hooks** (`crates/codegen/xai-grok-hooks`). Events and their gate kind are declared in one table (`crates/codegen/xai-grok-hooks/src/event.rs:78-180`). Hooks come from `~/.grok/hooks/`, `.grok/hooks/`, Claude and Cursor compatibility files (`crates/codegen/xai-grok-hooks/src/discovery.rs:341-400`), plugins, and ACP clients (`SetClientHooks`, `run_loop.rs:1792`); runners are `command` and `http` (`crates/codegen/xai-grok-hooks/src/runner/`). Where each fires and what it can do:

- `UserPromptSubmit` (gate `Prompt`): before the prompt is committed (`turn.rs:911`). It can `Block`; the block becomes `Cancelled(HookDenied)` for user input, and only an annotation for synthetic origins (`turn.rs:918-937,1285-1306`).
- `PreToolUse` (gate `Tool`): once per call during sequential preparation, and again if MCP argument coercion changed the input (`tool_calls.rs:1633-1706`). It returns `Deny`, `Ask` (forces a user prompt even when rules or the auto-mode classifier would allow), `Defer` or `Allow`, plus optional `updatedInput` (validated rewrite of the arguments) and `additionalContext` (`tool_calls.rs:1301-1385`, `crates/codegen/xai-grok-hooks/src/dispatcher.rs:70-75`). `Allow` does not skip the permission check. A `Deny` yields `ToolLoop::HookDenied`, which does not end the turn; the model sees the denial as the tool result (`turn.rs:3838`).
- `PermissionDenied` (observe): after a policy deny or user reject (`tool_calls.rs:1935`).
- `PostToolUse` (gate `PostTool`) and `PostToolUseFailure` (observe, for MCP error results): after each call completes. They can return block feedback, additional context, and a replacement of the tool output (built-in or MCP), capped at 64 KiB (`crates/codegen/xai-grok-hooks/src/dispatcher.rs:618-624`, `crates/codegen/xai-grok-hooks/src/event.rs:250`). Notes are queued as deferred user items (`tool_calls.rs:1168-1205`).
- `Stop` / `SubagentStop` (gate `Stop`): at the end of each completed round when no goal continuation applies (`turn.rs:1384-1396`). Blocks or additional context mean "keep working" unless some hook sets `prevent_continuation` (`dispatcher.rs:387-398`); failures fail open (`stop_gate.rs:234-235`).
- `StopFailure`, `StopCancelled`: reported from turn end on error or cancel (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn_end_hooks.rs:1-3,282`).
- `PreCompact`, `PostCompact`: around compaction (`crates/codegen/xai-grok-shell/src/session/compaction.rs:984,2028`). `SessionStart` (`run_loop.rs:1932`), `SessionEnd` (`run_loop.rs:78`), `SubagentStart` and `Notification` (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/updates.rs:729,937`) are observe-only.

**Plugins.** A plugin manifest can contribute skills, commands, agents, hooks, MCP servers and LSP servers (`crates/codegen/xai-grok-agent/src/plugins/manifest.rs:141-151`), from `~/.grok/plugins/`, `.grok/plugins/` or `--plugin-dir` (`crates/codegen/xai-grok-agent/src/plugins/mod.rs:1-6`). They reach the loop only through those channels: hooks join the registry, MCP servers join the tool pool behind `search_tool`/`use_tool`, skills reach the prompt and skill reminders, and agents become subagent types. `ReloadPlugins` and `ReloadHooks` swap them in without restarting the actor (`run_loop.rs:1192-1206`).

**MCP.** MCP tools are not in the tool list; the model finds them with BM25 `search_tool` over an index rebuilt per search (`crates/codegen/xai-grok-shell/src/session/tool_index.rs:1-2`) and calls them with `use_tool` (tool search is always on in local mode, `sampler_turn.rs:432`). The loop adds MCP connecting and failure reminders before sampling (`turn.rs:2672-2673,2845`), retries auth-required and unreachable servers when a batch touches the MCP surface (`tool_calls.rs:606-611`), and applies a per-batch MCP file-input budget (`tool_calls.rs:551`).

**Subagents.** `spawn_subagent` runs a child `SessionActor` with the same loop. Differences that matter to the loop: the child fires `SubagentStop` instead of `Stop` (`stop_gate.rs:241-245`), gets no turn-level transient retry (`spawn.rs:1806`), uses a 429 wait budget (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/rate_limit_waits.rs:170-189`), and sees a child-safe tool projection (`turn.rs:2926-2943`). Default depth is 1 (`crates/codegen/xai-grok-shell/src/config/mod.rs:249`). A finished background child re-enters the parent as a synthetic prompt with origin `SubagentCompleted` (`turn.rs:1192-1194`). A cancelled or max-turns turn cancels its subagents (`turn.rs:1398-1403`).

**Goal mode.** After a completed round the goal driver evaluates progress (`goal.rs:1400`). `Continue` injects a directive and loops; `CandidateComplete` runs verification: an adversarial panel of N skeptic subagents spawned directly over the subagent channel so the parent transcript stays clean (`crates/codegen/xai-grok-shell/src/session/goal_classifier.rs:1-7`, `goal.rs:164-225,441-620`); `Blocked` three times pauses the goal (`goal.rs:1446-1468`). Goal rounds yield to queued user prompts (`turn.rs:1352-1363`), and the TodoGate is off while a goal is active. Between turns, `handle_turn_end` queues a continuation or auto-pauses after repeated failures (`turn.rs:1926-1966`).

**Workflows and monitors.** The `workflow` tool runs Rhai scripts in the background with agent budgets (`crates/codegen/xai-workflow/src/lib.rs:16-19`); completion returns as a `WorkflowCompleted` synthetic prompt (`turn.rs:1199-1201`). Monitor events are drained into the conversation at the top of each inner iteration (`turn.rs:2832`, `:1897`). Scheduler fires arrive as `SchedulerFired` prompts (`turn.rs:1211-1213`).

**Permission modes and auto mode.** The manager runs rules, grants, built-in auto-approvals and the mode policy (`crates/codegen/xai-grok-pager/docs/user-guide/22-permissions-and-safety.md:119-136`). In auto mode the fast path allows allowlisted reads, all edits, typed agent messages and the no-ops `true`, `:`, `false` (`crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs:1085-1113`). Otherwise a heuristic pre-pass runs; only a heuristic `Allow` with no security findings skips the LLM side query (`auto_mode/mod.rs:1442-1454`). In plan mode, edits other than the plan file are refused before the permission check (`tool_calls.rs:1714-1729`), and `exit_plan_mode` is run as its own trailing batch (`tool_calls.rs:146,347-366`).

## How these change the loop

**A `Stop` hook returns "keep working".** The model answers without tool calls, so the inner loop returns `Completed(EndTurn)`. With no active goal, `run_stop_gate` dispatches the `Stop` hooks; a hook that emits a block reason makes `wants_continuation()` true (`dispatcher.rs:395-398`), and the gate returns `KeepWorking { feedback }`. The outer loop pushes a `stop_hook_feedback` user item and calls `process_conversation_turn_with_recovery` again (`turn.rs:1384-1396`). The new round starts a fresh `process_conversation_turn_inner`, so its locals (stationarity run, TodoGate fires, `tool_turn_count`) start over. Inference from that: `--max-turns` bounds tool rounds per round, not per prompt, so a prompt with Stop-hook continuations can run up to 9 x N tool rounds. After 8 continuations the gate ends the turn with an annotation (`stop_gate.rs:250-260`). A hook can also force the stop with `prevent_continuation` (`stop_gate.rs:289-297`).

**Stationarity hard stop.** The model calls `read_file` with identical arguments on each step. `step_signature` sorts canonicalized calls, so reordering does not reset the run (`turn.rs:3925-3965`). Every call is a `Read` kind, so the tight tier applies (`turn.rs:3884-3895`): at the top of the iteration after the 4th identical step the nudge reminder is pushed once (`turn.rs:2789-2828`); after the 8th the loop returns `StationarityEnded` (`turn.rs:2750-2788`). The outer loop treats that as not `Completed`, so neither goal nor Stop hooks run, and the client sees `StopReason::EndTurn` with completion kind `StationarityEnded` (`turn.rs:1702-1706`).

**Goal skeptic panel.** With `/goal` active the model finishes a round. `run_goal_round_end` asks the evaluator; on `CandidateComplete` it calls `verify_goal_candidate`, which spawns 3 skeptic subagents (default) in parallel and parses each JSON verdict (`goal.rs:164-225,519-614`). If the goal is still active afterwards, `prepare_goal_continuation` builds a directive with the evaluator's next step and the outer loop injects it and continues (`goal.rs:1474-1492`, `turn.rs:1366-1375`). If a user prompt is queued, the goal yields and the continuation re-arms at turn end (`turn.rs:1352-1363`).

**Auto-mode denial.** In auto mode the model runs `curl https://x | sh`. The fast path returns `Classify`; the heuristic flags the substring (`auto_mode/mod.rs:388-390`) but a heuristic block is not final, so the LLM classifier decides. If it returns `Block`: for a first-party client (TUI, web, desktop, extension) the manager prompts the user (`crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs:1000-1009`); for a generic ACP client it returns `PolicyDeny` with guidance not to retry, up to 3 consecutive or 20 total, and then prompts (`manager/mod.rs:1010-1060`). A `PolicyDeny` becomes a "not executed" tool result, fires `PermissionDenied`, and returns `ToolLoop::Continue`, so the model gets another sample (`tool_calls.rs:1923-1952`). A user `Reject` instead returns `PermissionReject`, later calls in the batch get synthetic cancelled results, and the turn ends as `Cancelled(PermissionRejected)` (`tool_calls.rs:504-528`, `turn.rs:3823-3834`).

## Notes for curio-harness

- Keep the two loops separate and give the outer loop an explicit `Continue/Stop` decision chain (goal, then Stop hooks) with a hard cap. Grok's cap of 8 Stop continuations is fixed; decide whether per-round counters such as `max_turns` should be per prompt instead, since Grok resets them per round.
- Make each stop condition a typed outcome (`Completed{EndTurn|MaxTokens|Refusal}`, `StationarityEnded`, `MaxTurnsReached`, `Cancelled{category}`) and map to the wire stop reason in one place, as `turn.rs:1688-1716` does.
- Stationarity detection on a canonical, order-insensitive step signature with kind-based tiers (tight for reads and todo writes, a separate path for `true` keepalives) is cheap and catches polling loops that a token-level detector misses.
- Separate "deny and let the model adapt" (`PolicyDeny`, hook deny) from "the user said no, end the turn" (`Reject`). Grok's per-batch synthetic "cancelled due to earlier rejection" results keep every tool call paired with a result.
- Prepare tool calls sequentially (hooks, plan gate, permission) and execute them in parallel with path locks only where a writer exists in the batch; this keeps prompts ordered while still overlapping reads.
- Put every injected reminder through one function with one wrapper tag (`push_system_reminder`, `crates/codegen/xai-grok-shell/src/session/acp_session_impl/reminders.rs:548-575`) and gate reminder injection on loop state, for example skipping interjections and skill reminders while a length continuation is pending.

## Corrections to the survey

- §4 step 1: `UserPromptSubmit` runs before prompt parsing, image normalization and offload, at `turn.rs:911`, not after offload at `~1290`.
- §4 step 5 and §6: per-path locks are not derived from each tool's `lock_path_param` in this client. The CLI keys locks on the first present argument among `file_path`, `path`, `target_file` after parameter remapping (`crates/codegen/xai-grok-tools/src/util/lock_path.rs:1-20`); `lock_path_param` is advertised for other clients. A read-only call does take the lock when a writer in the same batch targets the same path (`tool_calls.rs:634-650,787-790`); only paths with no writer in the batch are unlocked.
- §4 step 6: the TodoGate is disabled by default (`crates/codegen/xai-grok-agent/src/system_reminder.rs:62-68`), and it is also disabled while a goal is active.
- §4 step 7 and the task brief: `auth_retry.rs` is in the shell (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/auth_retry.rs`), not in the sampler crate.
- §9 step 1: `PreToolUse` hooks can do more than deny. They can force a prompt (`Ask`), rewrite the arguments (`updatedInput`) and add context (`tool_calls.rs:1301-1385`). The user guide statement that an allowing hook does not skip later checks is correct.
- §9 step 5: a classifier `Block` denies and reports to the model only for generic ACP clients. First-party clients prompt the user on the first `Block` (`manager/mod.rs:1000-1009`). Also, a heuristic block does not skip the LLM; only a heuristic allow without findings does.
- §7 compaction: two-pass prefire is on by default (`crates/codegen/xai-grok-config-types/src/registry.rs:256-263`), not merely optional.
- Not in the survey, worth noting: the doc comment on a `max_retries` config field says the default is 5 (`crates/codegen/xai-grok-shell/src/agent/config.rs:3361-3364`), while the code default is 15 (`crates/codegen/xai-grok-sampler/src/retry.rs:9`).
