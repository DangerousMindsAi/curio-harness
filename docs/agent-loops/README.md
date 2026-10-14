# Agent Loops

How the core agent loop works in opencode, DeepSeek Harness and Grok Build, written as groundwork for curio-harness. This builds on the [architecture survey](../harness-survey/README.md) and goes deeper on one subsystem.

- [opencode.md](opencode.md): an inbox drained one step at a time, each step one model call rebuilt from SQLite, with tools dispatched while the model is still streaming.
- [deepseek-harness.md](deepseek-harness.md): a small replaceable driver (`ReactLoopAgent`) whose policy lives almost entirely in plugins on typed waterfalls.
- [grok-build.md](grok-build.md): a session actor running an outer round loop (goal, Stop hooks) around an inner sampling loop with heavy harness-side control.

Each document has the same sections: plain-language pseudo-code for the whole loop with every hook and config point marked, a map from each part to the source, a table of what is configurable, the plugins and hooks that touch the loop, worked examples, notes for curio-harness, and corrections to the survey found while reading the code. Claims cite `path:line` at the same commits the survey used: opencode `8ffbcc2`, deepseek-harness `d743267`, grok-build `2bdd1d6`.

## The three loops side by side

| | opencode | DeepSeek Harness | Grok Build |
|---|---|---|---|
| Entry point | `Session.prompt` writes an inbox row and wakes a per-session coordinator | `followup` / `steer` / `inject` splice into a durable two-list inbox | `Prompt` command to the `SessionActor`, queued; one turn task at a time |
| Nesting | drain → step → attempt | turn → step → request attempt | turn → round → sampling iteration |
| Model input | Rebuilt from SQLite history every step | Derived from the session log every attempt (`deriveMessages`) | Built by the chat-state actor from in-memory conversation |
| Tool dispatch | Eager: each call forked as soon as it streams in | After the stream: parallel-safe calls in a pool, exclusive calls as barriers, results committed in model order | After the stream: prepared one at a time (hooks, permission), then run in parallel with per-path locks |
| Mid-turn user input | `steer` items promoted at the next step boundary; `queue` waits for idle | `steer` to the next step, `followup` to the next turn | Interjections drained at safe points in the inner loop |
| Continue condition | At least one local tool ran | The model made tool calls, or the next-step inbox is non-empty | The model made tool calls, or a reminder, interjection, goal or Stop hook asks for more |
| Step cap | Per-agent `steps` (unset by default); the last step forces `toolChoice: "none"` | None in the loop; goal mode caps rounds at 256 | `--max-turns` / agent `maxTurns` per round (unlimited by default); Stop-hook continuations capped at 8 |
| Repetition control | None in V2 (V1's `doom_loop` permission survives only in config migration) | Advisory reminders at 3, 5 and 8 identical calls (plugin) | Fixed stationarity nudge and hard stop on a canonical step signature |
| Length truncation | Synthetic continue message; the step fails on the third truncated reply in a row | `max-tokens` ends the turn and its tool calls are not run | Length salvage budget (off by default); truncated tool-call samples capped at 5 |
| Context overflow | Auto compaction before each step; one overflow compaction per step | Compaction plugin on `agent/pre-step` and on `agent/request-error`, then retry | Auto compaction (two-pass prefire on by default) before sampling; recovery compaction on overflow errors |
| Retry | 10 retries, fixed policy; `session.retry` hook can override | `llm-retry` plugin on `agent/request-error`, provider `retryPolicy` (5 by default) | Sampler retries (15 by default), turn-level transient resubmit, doom-loop resample, auth retry |
| Extension model | Mutable hook events plus catalog transforms | Cordis waterfalls and serial events; the loop itself is a replaceable service | Command and HTTP hooks (Claude Code and Cursor compatible), plugins that contribute hooks, MCP, skills and agents |

## What the comparison suggests

Every loop has the same core (build request, stream, run tools, repeat while tools were called), and the differences are about three questions curio-harness will have to answer early.

1. **Where does policy live?** opencode keeps step policy in one runner function with a closed outcome union and lets hooks reshape requests. DeepSeek keeps the driver at about 700 lines and puts retry, compaction, context and continuation in plugins. Grok puts all of it in the turn file, which makes the control flow easy to follow but hard to extend without editing the loop.
2. **How is continuation expressed?** DeepSeek's rule ("another step if the next-step inbox is non-empty") lets several plugins request more work without ordering conflicts. opencode and Grok decide continuation in code from the step outcome, with Grok adding an explicit Stop gate in an outer loop.
3. **How much does the harness police the model?** The range runs from none in the loop (DeepSeek) through a configurable step limit (opencode) to stationarity detection, TodoGate, length salvage, Stop hooks and goal verification (Grok). Grok's counters reset each round, so its limits bound a round, not a prompt; whichever way curio-harness goes, the scope of each limit should be a deliberate choice.
