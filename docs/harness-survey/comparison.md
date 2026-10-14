# Three Harnesses Compared

*How opencode, DeepSeek Harness and Grok Build put an agent together, where they agree, and what each does that the others don't.*

This compares the three architecture reports in this folder: [opencode](opencode.md), [deepseek-harness](deepseek-harness.md) and [grok-build](grok-build.md). Claims here are summaries; the reports carry the `path:line` citations. All three are snapshots from September and October 2026 and are moving quickly, so treat version-specific details as dated.

## At a glance

| | opencode | DeepSeek Harness (`dsh`) | Grok Build (`grok`) |
|---|---|---|---|
| Upstream | anomalyco/opencode (originally sst/opencode) | DeepSeek | xAI (read-only mirror) |
| Language | TypeScript on Effect v4, Bun | TypeScript on Cordis, Node | Rust on tokio |
| License | MIT | MIT | Apache-2.0 |
| Size (non-test) | ~590k lines, ~53 packages | ~418k lines, 331 packages | ~1.4M lines, 107 crates |
| Maturity | V2.0.29, post-rewrite | 0.2.1-alpha.2 | Shipping product, mirror only |
| Organizing idea | Client/server with an event-sourced core | Everything is a plugin | Protocol-separated runtime with heavy harness-side loop control |
| Process model | One shared background server per user; every UI is an HTTP/SSE client | One `dsh` launcher; each surface is a profile (stack of YAML patches) | One binary; the TUI talks ACP to an in-process agent; optional per-machine "leader" |
| Interactive UI | TUI (OpenTUI + Solid), web, Electron | Web (React) and Electron; no TUI | TUI (ratatui), `--minimal` scrollback mode |
| Editor/automation | ACP, generated HTTP SDK, in-process SDK, GitHub Action | ACP, JSON-RPC SDK (TypeScript and Python) | ACP over stdio or WebSocket, headless `-p` |
| Session store | SQLite event table plus projections | JSONL event log (zstd), versioned format with migrations | Per-session files (`updates.jsonl`, `chat_history.jsonl`, …) via a persistence actor |
| Loop shape | One LLM call per step, history reloaded from SQLite | ReAct loop with typed waterfall hooks at every stage | Nested round/sample loop with reminders, gates and stop hooks |
| Step limit | Per-agent `steps` | None in the loop; plugins advise | `max_turns` plus stationarity hard stop |
| Edit tool | Exact replace with tiered fuzzy matching; Codex `patch` for GPT models | Exact string replace with read-before-write policy | Several dialects, including content-hash **hashline** anchors |
| Large tool surfaces | Code Mode: catalog plus one `execute` tool | PTC: one `run_code` tool over a generated SDK | Deferred MCP tools behind BM25 `search_tool` / `use_tool` |
| OS sandbox | None | Default on, fails closed (bwrap/Landlock, Seatbelt, Windows ACL) | Off by default; whole-process Landlock/Seatbelt, plus per-command Seatbelt |
| Approval model | Rules (default allow `*`), tools request permission themselves | Sandbox first; approval only to widen the sandbox | Rules and modes, plus an LLM-judged auto mode |
| Undo | Git shadow-repo snapshots per step | Fork from the log; no file rewind | Rewind checkpoints across files, hunks and git |
| Providers | Own library, ~45 wire protocols | DeepSeek native, others through pi-ai | xAI native; Chat Completions, Responses and Messages wires |

## The shared skeleton

Underneath the differences, the three harnesses have converged on more than they disagree about. Each one runs the same basic loop: assemble a request from durable history, stream it, dispatch tool calls (in parallel where safe), append results, and go again until the model stops calling tools. Each treats the persisted session as the thing the loop reads from, so resume, fork and crash recovery fall out of the storage design instead of being bolted on. Each puts a protocol between the agent and its UIs; opencode uses HTTP and SSE, DeepSeek Harness uses its own JSON-RPC and generated typed RPC, and Grok Build uses ACP even inside a single process. All three speak ACP to editors.

They also agree on several smaller conventions that are worth inheriting rather than reinventing. Instruction files follow the `AGENTS.md` convention (DeepSeek and Grok also read `CLAUDE.md`). Skills are `SKILL.md` folders advertised as a short catalog and loaded on demand. Subagents are first-class, with explore and plan variants, background execution, and the ability to continue a finished child. Tool output is capped with a head-and-tail cut and the full text spilled to a file whose path is given to the model. Context overflow triggers summarizing compaction, with the original transcript kept on disk.

Prompt caching shapes all three designs more than any README admits. opencode and DeepSeek both append instruction and tool changes as new messages instead of rewriting the system prompt, and both keep the tool list stable when a mode would otherwise remove tools. Grok aligns side calls (titles, classifiers, compaction) with the main request's cached prefix and hides MCP tools behind a search tool so servers can come and go without touching the tool list. In our reading, cache stability is the single most consistent design constraint across the three.

## Where they diverge

### Composition

The biggest architectural difference is how each harness is assembled.

opencode is a layered application: Schema, then Core and Protocol, then Server, then generated clients. Its core is built from an ordered list of internal plugins, and external plugins get the same transforms and hooks, but the shape of the system is fixed by the packages. Effect v4 runs through all of it, which gives typed services and errors at the cost of a steep learning curve.

DeepSeek Harness has no privileged core. The agent loop, tool registry, session log, prompt assembler and LLM runtime are each a Cordis service that any YAML patch can replace, and every capability is split into a Service Definition, Providers and Consumers. A product surface is an ordered stack of patches over an empty root. This makes the system unusually swappable (moving filesystem, subprocess and sandbox providers to SSH moves every tool with them), and it also means understanding any behavior requires knowing which plugins a profile composed.

Grok Build is a conventional Rust workspace organized by layer, with the runtime decomposed into actors (chat state, sampler, persistence, hunk tracker, signals) on a single-threaded `LocalSet`. Extensibility comes through hooks, plugins in the Claude Code layout, MCP and Rhai workflows rather than through replacing core services.

### Process topology

opencode runs one elected background server per user that serves many directories and many clients at once; a TUI, a browser tab and an editor can all watch the same session. Sessions can even move between directories or worktrees mid-flight. DeepSeek Harness runs one process per profile, with the web and desktop hosts serving a browser UI and headless, SDK and ACP profiles running single tasks. Grok Build runs in-process by default but can promote itself to a per-machine leader over a Unix socket, and its in-progress hub/ToolServer split lets the model loop run in one place while tools run on a remote workspace server.

### Loop control

The three disagree most about how much the harness should police the model. DeepSeek trusts the model: there is no step cap in the loop, and runaway protection is advisory reminders and cooperative timeouts. opencode sits in the middle with a per-agent step limit, length-continuation, and retry classification. Grok Build invests heavily here. It canonicalizes each step's tool calls to detect repetition, nudges and then hard-stops, salvages length-truncated output, gates completion on unfinished todos, runs an idle "laziness" classifier, accepts stop-hook verdicts that keep the agent working, and receives doom-loop signals from the inference server itself. Its `/goal` mode only accepts completion after a panel of skeptic subagents agrees.

### Safety

The approaches here are almost orthogonal. DeepSeek Harness is sandbox-first: commands run inside an OS sandbox by default and fail closed if no backend works, and the model asks for approval only when it wants a wider sandbox. opencode has no OS sandbox; it relies on tree-sitter-parsed permission rules (default allow everything except `.env` reads) and on git snapshots for recovery. Grok Build offers an optional whole-process sandbox and a per-command one, but its distinctive piece is the auto-mode classifier: a fast path, then a deterministic bash heuristic, then an LLM judge that treats `AGENTS.md` and the transcript as untrusted and returns a strict JSON verdict.

### Persistence

All three are event-sourced in spirit, with different levels of rigor. opencode writes durable events to SQLite and projects them into read models, records each tool call before its side effects, and resumes crashed runs at startup. DeepSeek is the strictest: every model-visible input must be reconstructable from the log, each request is derived from the log just before sending and then frozen, compaction is a "replace" view that keeps the originals, and the file format is versioned with one-step migrations that never overwrite earlier generations. Grok Build stores several parallel files per session behind a persistence actor with a written power-loss contract, and adds three-domain rewind (files, hunk attribution, git).

## What is unique about each

### opencode

- **A durable inbox and a stateless loop.** Prompts enter an inbox (`steer` or `queue`), and each step is one LLM call rebuilt from SQLite. There is no in-memory tool loop to lose, so crashes and multiple clients are handled by the same mechanism.
- **One server, many clients, many directories.** The elected per-user server, SSE fan-out with per-connection back-pressure, and session moves between worktrees are unlike the other two.
- **Its own LLM library.** `@opencode/ai` covers about 45 wire protocols, including WebSocket transports and provider-native compaction, and keeps the Vercel AI SDK only as a fallback.
- **Eager tool execution.** Tool calls start running as soon as each call finishes streaming, before the response ends.
- **Per-model adaptation as plugins.** Model families get their own system prompts, and GPT models get a Codex-style `patch` tool instead of `edit`.
- **Code Mode.** MCP and auxiliary tools are reachable through one `execute` tool running a small pure interpreter.
- **Enterprise policy.** Last-match-wins policies over providers, tools and integrations, with MDM-managed config taking final precedence.
- **Runs on Cloudflare workerd.** Core can boot inside a Durable Object.

### DeepSeek Harness

- **Everything is a plugin, including the loop.** Profiles are YAML patch stacks; any service can be replaced by id.
- **"Model-visible ⟺ logged" as an enforced invariant.** Replay is exact, and the same log feeds UI, fork, resume, search and tests.
- **Released-format discipline.** Versioned session format, adjacent migrations, immutable file generations and acknowledgement records for persistence-type changes.
- **PTC mode.** The model writes TypeScript (or Python) against a generated SDK in one `run_code` tool; every inner call still goes through the full policy pipeline and is logged.
- **Sandbox-first permissions** with fail-closed backends on Linux, macOS and Windows.
- **Heterogeneous subagents behind one seam**, including real Claude Code and Codex runtimes as children.
- **KV-cache effect as documented design.** Every package README must state how it affects the cached prefix.
- **Testing by replaying recorded sessions through shipped profiles**, backed by a 100% per-file coverage gate.

One caveat worth knowing: the official DeepSeek adapter uploads the session log and plugin list with each request by default.

### Grok Build

- **Tool dialects.** One runtime carries Grok-native, concise, hashline, Codex-port and opencode-port tool sets, chosen per session, per subagent or by remote setting. The apparent purpose is running each model with the tool format it was trained or evaluated with.
- **Hashline edits.** Lines carry short content-hash anchors; the model edits by anchor instead of quoting old text, anchors are validated against a snapshot, edits apply bottom-up, drift is recovered within ±15 lines, and fresh anchors come back after each edit.
- **The auto-mode permission classifier** described above.
- **Harness and model co-design.** The inference server flags doom loops mid-stream, and request headers tell it how many compactions remain.
- **Anti-laziness machinery** and skeptic-panel goal verification.
- **ACP everywhere**, including inside the TUI, plus the optional leader and the remote ToolServer split.
- **Deep Claude Code and Cursor compatibility.** It reads their config, rules, hooks, plugins and agents, and can import Claude Code sessions.
- **Rhai-scripted workflows** for multi-agent orchestration such as `/deep-research`.

## Three answers to the same problem

Several problems are solved differently by each harness, and these are the decisions curio-harness will have to make for itself.

**Large tool surfaces.** opencode's Code Mode and DeepSeek's PTC both replace many tools with one code-execution tool; opencode uses a confined interpreter, DeepSeek a sandboxed Node process with a generated typed SDK. Grok keeps native tool calls but defers MCP tools behind search. Code execution scales better and composes calls; search keeps the model on the tool-call format it knows best.

**File edits.** Exact string replacement (DeepSeek, opencode's default) is simple and model-agnostic, and opencode shows how much fuzzy matching it needs to be reliable. Patch envelopes suit models trained on Codex. Hashline anchors remove the "old string not found" failure class at the cost of a custom read format. Grok's answer is to support all of them and choose per model, which is the most flexible and the most code.

**Approval.** Sandbox-first (DeepSeek) minimizes prompts while keeping a hard boundary. Rule-based permissions with snapshots (opencode) minimize friction and rely on undo. An LLM judge layered over a deterministic heuristic (Grok) automates the judgment call, and it needs careful prompt-injection hygiene to be safe.

**Runaway loops.** Trusting the model (DeepSeek), a step budget (opencode), or extensive harness-side detection (Grok). Grok's machinery reflects running at scale against its own models; how much of it a smaller harness needs is an open question.

## Implications for curio-harness

These are starting points for discussion rather than decisions.

1. **Make the session log the source of every request.** All three converge here, and DeepSeek's strict version (derive each request from the log, keep compaction as a view over it) makes resume, fork, replay testing and multi-client UIs cheap later.
2. **Put a protocol between the agent and every UI from the start.** ACP is the common denominator across all three and gives editor integration for free.
3. **Treat prompt-cache stability as a design constraint.** Append instruction and tool deltas; keep the tool list stable across modes.
4. **Record each tool call before it runs and always write a result,** synthetic if needed, committed in model order.
5. **Make the model-facing tool surface data.** Grok's renamable tool ids and templated prompts make dialect experiments cheap, which matters if curio-harness will run more than one model family.
6. **Pick a sandbox story early.** Retrofitting one is hard; DeepSeek's fail-closed, escalate-on-request design is the cleanest of the three.
7. **Read other harnesses' config formats.** `AGENTS.md`, `CLAUDE.md`, `SKILL.md`, `.mcp.json` and Claude Code hooks are close to de facto standards.
8. **Be deliberate about scale.** Each of these is several hundred thousand lines or more. DeepSeek's core loop is about 2.5k lines; the rest is policy, surfaces and process. A small harness can take the invariants without the machinery.
