# Crash Recovery

What happens in opencode, DeepSeek Harness and Grok Build when the agent process dies in the middle of a turn, and whether the session can be resumed. This builds on the [architecture survey](../harness-survey/README.md) and the [agent loop docs](../agent-loops/README.md), and uses the same commits: opencode `8ffbcc2`, deepseek-harness `d743267`, grok-build `2bdd1d6`.

Citations are `path:line`. opencode paths are under `packages/core/src/` unless they start with `packages/`. Grok Build paths are under `crates/codegen/`. DeepSeek Harness paths are from the repository root.

## Short answer

All three can resume a session after a crash. None of them loses the conversation before the crash, none of them re-runs a tool that was interrupted, and none of them rolls back files on disk. They differ on one big question: does the work carry on by itself?

- **opencode carries on by itself.** When the server starts again it finds every session that was mid-turn, tells the model "the server restarted while you were working, continue from where you left off", and runs the next step without anyone typing anything.
- **DeepSeek Harness and Grok Build stop and wait.** When the session is next opened, the unfinished turn is closed and marked as interrupted, and the model is told what happened. Nothing runs until the user sends another message.

| | opencode | DeepSeek Harness | Grok Build |
|---|---|---|---|
| Resumes after a crash? | Yes, automatically when the server restarts | Yes, when the user or a client reopens the session | Yes, when the user reopens the session (`-c`, `-r`, ACP load) |
| Interrupted turn | Continues with a "server restarted, continue" message | Closed as "interrupted"; waits for the next message | Closed as "interrupted"; waits for the next message |
| Tool that was running | Marked failed: "Tool execution interrupted" | Marked failed: "outcome unknown, verify before retrying" | Given a placeholder result: "cancelled by the user (not executed)" |
| Half-streamed model text | Lost (only finished text blocks are kept) | Lost (no checkpoint inside a model call) | Shown in the replayed transcript, but not given to the model |
| When work is forced to disk | Every event, in a SQLite transaction | Before each model call and before each tool runs | When the prompt is accepted and when the turn ends |
| Guard against two processes resuming one session | Relies on only one managed server running; no lock in the database | File lock; a second writer is refused | Interrupt check runs only if no live process has the session loaded |
| Crash loop protection | Gives up after 10 resumes of the same turn | Not needed (no automatic resume) | Not needed (no automatic resume) |
| File changes from the crashed turn | Kept; can be undone through the normal revert | Kept; no undo feature | Kept; likely not undoable with `/rewind`, because the rewind point is written at turn end |
| Background shells | Reported as "cancelled because the server restarted" | Die with the process | Forgotten on reload; processes may keep running untracked |
| Subagents | Background ones are resumed; foreground ones fail | Parent's call becomes "outcome unknown"; the child session can be reopened | Marked cancelled ("interrupted by process restart") |

## The general idea all three share

Each harness writes the session as a log of things that happened (the user said this, the model asked for this tool, the tool returned that). Recovery is reading that log back and patching up the loose ends at the point where it stops. Those loose ends are the same in every harness:

1. **A tool call with no result.** The log says the model asked for a tool, and maybe that the tool started, but there is no result. The tool may have finished its work (written a file, pushed a commit) and died before the result was saved, or it may never have run. The harness cannot tell, so it writes a stand-in result so the conversation is valid again, and the three differ mainly in what that stand-in tells the model.
2. **A model reply that was still streaming.** None of the three keeps half a reply for the model. Whatever was mid-stream is dropped.
3. **A turn that never ended.** It has to be closed, or else continued.

How much can be lost depends on how often the harness forces its log to disk. All three accept losing the last moments of the turn that was running, and none of them can lose a finished turn.

## opencode

**What happens after a crash, in order:**

1. The server process dies mid-turn. The session row still carries a "this session is running" marker, written when the run started (`session/execution.ts:76-86`, `session/store.ts:205-216`). A clean finish, a failure or a user interrupt clears it (`session/execution.ts:119-146`), but a crash does not. A graceful shutdown deliberately leaves it in place so the turn also resumes after a normal restart (`session/execution.ts:128-134`).
2. On the next start, a recovery sweep lists every top-level session still carrying the marker (`session/execution/restart.ts:193-231`). The managed server, the workerd entry point and the SDK host all run it at boot (`packages/server/src/process.ts:107-110`, `packages/server/src/fetch.ts:47`, `packages/sdk/src/internal/host.ts:56`).
3. For each one it adds 1 to a saved resume counter. If that turn has already been resumed 10 times, it gives up and marks the run failed with "Execution was interrupted repeatedly and will not be resumed automatically" (`session/execution/restart.ts:18-33`, `:82-90`). This stops a turn that crashes the server every time from crashing it forever.
4. Otherwise it adds a message to the conversation: "The server restarted while you were working. Continue from where you left off without repeating completed work." (`session/execution/restart.ts:15-16`, `:92-97`), and starts the loop again even though no new user input is waiting (`session/run-coordinator.ts:124-134`).
5. Before the next model call, the loop cleans up: any tool still marked "running" or "streaming" is marked failed with "Tool execution interrupted: <name>", and any summarisation that was running is marked failed (`session/runner/llm.ts:71-72`, `:332-385`). Tools are never re-run by the harness; the model sees the failure and decides.
6. The model gets the conversation as it was saved: completed text, the failed tool calls and the restart message. Text that was mid-stream was saved as an empty block and is left out of the request (`session/message-updater.ts:289-292`, `session/runner/to-llm-message.ts:207-211`).

**How durable the log is.** Every event commits in its own SQLite transaction that also updates the session and message tables (`bus.ts:315-433`). The tool-started record commits before the tool is launched (`session/runner/step.ts:104`, `:121-133`), and the result commits after it returns. SQLite runs in WAL mode with `synchronous=NORMAL` (`database/database.ts:39-44`): a killed process loses nothing that was committed, while a power cut or OS crash can lose the last few commits.

**Two processes.** There is no lock in the database. The sweep trusts that the previous owner is dead because the managed server only starts a successor after confirming the old process has exited, and only one managed server can be registered (comment at `session/execution/restart.ts:55-61`). Embedders that call the sweep themselves get no such guarantee; two of them sharing one database could both resume the same session (inferred from the code; nothing prevents it).

**Side effects.** The code says outright that recovery is "at-least-once" for side effects (`session/execution/restart.ts:52-53`). A tool that finished its work but died before its result was saved is reported as interrupted, and the model may run it again.

**Background work.** Background shells that were running come back as "Command cancelled because the server restarted"; the process itself is not revived (`session/execution/restart.ts:101-136`). Background subagents are resumed with their own counter, and the parent is told once (`:138-190`). A subagent the parent was waiting on directly is failed like any other tool (`session/runner/llm.ts:369-378`).

**Files.** Per-step git snapshots exist, but they feed the user's revert command (`session/revert.ts:41`, `:67`), not crash recovery. Files are not rolled back after a crash.

**Tests.** Recovery is tested by writing the "crashed" state into the database and starting the sweep, not by killing a real process: `packages/core/test/session-execution.test.ts` (resume once `:244`, give up `:291`, shells `:379`, subagents `:789`) and `packages/core/test/session-runner.test.ts:4402-4640`.

## DeepSeek Harness

**What happens after a crash, in order:**

1. The process dies mid-turn. Because of the save points described below, the log on disk ends somewhere after the last model call or tool launch.
2. Nothing happens until someone opens the session again. That is a user or client action: `dsh --profile headless --session-id <id> "task"` (`packages/bundle/headless/src/startup.ts:44`, `:51`), ACP `session/resume` (`packages/acp/acp/src/index.ts:243-261`), or the web API reopening a cold session when it needs it (`packages/api/session-controller/src/agent.ts:169-200`). Sessions only resume at startup if they are listed by id in the config (`packages/core/agent-loop/src/index.ts:345-357`, `:392-419`), and the scheduler reopens a session when a reminder for it falls due (`packages/schedule/schedule/README.md:12`).
3. On open, the storage layer repairs only the file: a half-written last line or compressed block is dropped, and any complete records inside a torn block are recovered (`packages/session/session-persistence-jsonl/src/index.ts:792-795`, `:989-1020`). A checksum failure inside a complete block is treated as corruption and refused.
4. The agent loop then closes the unfinished turn by appending records (`packages/core/agent-loop/src/index.ts:875-899`, `packages/core/session/src/repair.ts:53-98`):
   - every tool call without a result gets an error result;
   - an open step gets an end record;
   - the turn gets an end record with the reason "interrupted".
5. The error result depends on how far the tool got (`packages/core/session/src/repair.ts:30-37`):
   - if the tool was recorded as started: "Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
   - if it was never recorded as started: "Retry it if it is still needed."
6. The agent then sits idle. Messages the user had queued for later survive, because the queue is itself stored in the log (`packages/core/agent-loop/src/inbox.ts:20-54`), and they run with the next turn. The model sees the repaired history on that turn.

**How durable the log is.** A small plugin, `session-checkpoint-policy`, forces the log to disk at three moments: before every model call, before every top-level tool runs, and at the start of every step, which saves the previous step's reply and tool results (`packages/session/session-checkpoint-policy/src/index.ts:29-82`). Forcing means writing and fsyncing (`packages/session/session-persistence-jsonl/src/index.ts:1334-1372`). If that write fails, the model call or tool does not happen (`packages/session/session-checkpoint-policy/README.md:52`). Between those moments, events are batched for up to 200 ms (`packages/session/session-persistence-jsonl/src/storage.ts:36`), so a message queued in the last fraction of a second before a crash can be lost (inferred).

This is the most deliberate design of the three: because the tool-started record is guaranteed on disk before the tool runs, the harness can always tell "might have run" apart from "definitely did not run", and it tells the model which one it is.

**Two processes.** Opening a session for writing takes an OS file lock (`packages/session/session-persistence-jsonl/src/lease.ts:3-8`, `:34`). A second process is refused immediately with an "already owned" error (`:82`, `:96`). The lock disappears with a crashed process, so recovery never waits on a stale lock.

**Background work.** Each subagent is its own saved session and can be reopened; the parent's call to it becomes "outcome unknown" (`packages/subagent/subagent/README.md:61-65`, `:200`). Background jobs and terminals live in memory only and die with the process (`packages/jobs/jobs-local/README.md:32`, `packages/terminal/terminal/README.md:134`). Scheduled follow-ups are saved and fire after a restart, at most once per missed task, and may repeat one if the crash came at the wrong moment (`packages/schedule/schedule/README.md:153-154`).

**Files.** There is no file snapshot or undo at all. The checkpoint README asks tools to use the call id as an idempotency key when they need safe retries (`packages/session/session-checkpoint-policy/README.md:114`).

**Tests.** The only one of the three that kills a real process: `packages/session/session-checkpoint-policy/tests/crash-recovery.e2e.ts:39-119` sends SIGKILL at the model call and at the tool call, then checks the saved request and the "outcome unknown" repair (skipped on Windows). `packages/session/session-persistence-jsonl/tests/lease.two-process.e2e.ts:34` checks the lock across two processes. Repair on resume is in `packages/core/agent-loop/tests/resume.spec.ts:458-976`.

## Grok Build

**What happens after a crash, in order:**

1. The process dies mid-turn. The user prompt was forced to disk before the first model call (`xai-grok-shell/src/session/acp_session_impl/turn.rs:1238-1260`), but tool results and streamed text since then were only buffered.
2. Nothing happens until the user reopens the session with `-c`, `-r <id>` (`xai-grok-pager/src/app/cli.rs:542-577`) or a client sends ACP `session/load` or `session/resume` (`xai-grok-shell/src/agent/mvp_agent/session_setup.rs:923`, `:1997-2051`).
3. On load, unreadable lines are skipped, and a damaged chat file is copied aside once as `chat_history.jsonl.corrupt` (`xai-grok-shell/src/session/storage/jsonl/mod.rs:954-1082`).
4. The harness checks whether the event log ends with a turn that started and never ended (`xai-grok-shell/src/session/interrupted_turn.rs:92-147`). It does this only when no live process already has the session loaded (`xai-grok-shell/src/agent/mvp_agent/session_setup.rs:1155-1165`).
5. If so, it closes the turn with the reason "interrupted" and the transcript line "Grok stopped before this turn finished (the agent process exited or was restarted). Committed tool results were kept." (`interrupted_turn.rs:9-12`, `session_setup.rs:1570-1606`). It also queues a note for the model: "The previous turn was interrupted (the system exited or was restarted). Work before this message may have been lost." (`interrupted_turn.rs:28-31`).
6. When the conversation is loaded, any tool call without a result gets a placeholder: "Tool execution was cancelled by the user (tool `X` was not executed)" (`xai-chat-state/src/actor/state.rs:195-213`, `xai-grok-sampling-types/src/conversation.rs:2049-2058`). That wording is wrong after a crash: the user did not cancel anything, and the tool may well have run.
7. The session then waits. The UI shows an "Interrupted" marker (`xai-grok-pager/src/app/turn_completion.rs:59`, `:115`), and the note to the model goes out with the user's next message.

**How durable the log is.** All writes go through one writer task with a written promise about power loss (`xai-grok-shell/src/session/persistence.rs:1-14`): ordinary writes may lose the tail of the turn that is running, and anything a caller waits on is on disk when the wait returns. The waited-on save points are when the prompt is accepted, when the turn ends, after a user-run shell command and at shutdown (`xai-grok-shell/src/session/acp_session_impl/turn.rs:1404-1419`, `xai-grok-shell/src/session/acp_session_impl/tool_dispatch.rs:345`, `xai-grok-shell/src/session/acp_session_impl/run_loop.rs:215`). There is none before or after each tool. So a crash late in a long turn can lose every tool result from that turn, and those calls come back as "not executed".

**Which file wins.** Code comments call `updates.jsonl` the source of truth and `chat_history.jsonl` a cache (`xai-grok-shell/src/session/storage/mod.rs:254-256`). In practice, loading reads `chat_history.jsonl` and rebuilds it from `updates.jsonl` only if it is missing or empty (`storage/jsonl/mod.rs:217-225`). Streamed text is written to `updates.jsonl`, so the user sees a half-finished reply in the replayed transcript, while the model does not (inferred from the write paths).

**Avoiding the crash in the first place.** Grok can run the agent in a separate background "leader" process that outlives the terminal UI, so a UI crash does not stop the turn and reconnecting picks it up live (`xai-grok-shell/src/leader/mod.rs:1626-1630`). It is off by default (`xai-grok-pager/src/app/mod.rs:493-525`).

**Background work.** Subagents that were running are marked cancelled, "interrupted by process restart", unless a finished result survived, in which case that is delivered (`xai-grok-shell/src/agent/subagent/mod.rs:2403`, `:2534-2640`). The background task list is emptied on load (`session_setup.rs:1631-1635`); shell tasks run in their own process group, so they probably keep running with nothing tracking them (inferred, not tested).

**Files.** Rewind checkpoints are for the user's `/rewind`, not crash recovery. The per-turn rewind point is written at turn end (`turn.rs:1405-1419`), so a turn killed midway likely has none, and its edits cannot be rewound (inferred). The on-disk copy under `.grok/rewind-checkpoints` is off by default and not yet used for restoring (`xai-grok-workspace/src/session/checkpoint.rs:90-94`, `xai-grok-workspace/src/session/checkpoint_store.rs:1-12`).

**Tests.** Repair of dangling tool calls (`xai-chat-state/src/actor/tests.rs:2245`, `:2410`), torn lines (`xai-grok-shell/src/session/storage/jsonl/tests.rs:2094-2339`) and orphaned subagents (`xai-grok-shell/src/agent/subagent/tests/rest.rs:1420`). There is no test for the interrupted-turn check itself and none that kills the agent mid-turn.

## Notes for curio-harness

1. **Decide early whether a crashed turn continues by itself.** opencode's automatic resume is convenient for long unattended runs and needs two safeguards it already has: a cap on resumes per turn, and certainty that only one process resumes a session. DeepSeek and Grok avoid both problems by waiting for the user, at the cost of stalling unattended work. A middle path is to resume automatically only for sessions started in an unattended mode.
2. **Save "tool started" to disk before the tool runs.** This is what lets DeepSeek tell the model "might have run" apart from "never ran", which is the single most useful fact after a crash. opencode records it too but tells the model only "interrupted"; Grok does not force it to disk and tells the model "not executed", which can be false.
3. **Word the stand-in result honestly.** DeepSeek's text ("outcome unknown, verify before retrying, do not retry blindly") is the one to copy.
4. **Lock the session file.** An OS file lock, as in DeepSeek, is simple, disappears with a crashed process, and removes the two-resumers problem without a lease protocol.
5. **Think about undo for the crashed turn.** None of the three can reliably undo the file edits of a turn that crashed. Taking the file snapshot at the start of a step (opencode) rather than the end of a turn (Grok) covers this.
6. **Background processes need an owner after a restart.** All three lose track of background shells. Recording their process ids in the session would at least let a resumed session report or stop them.
7. **Test with a real kill.** Only DeepSeek SIGKILLs a process in its tests; the others simulate the crash by writing state directly, which cannot catch a save point in the wrong place.
