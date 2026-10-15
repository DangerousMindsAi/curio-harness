# OpenCode (sst/opencode fork, V2 snapshot): architecture report

Snapshot: [DangerousMindsAi/opencode](https://github.com/DangerousMindsAi/opencode) at `8ffbcc2` (2026-10-14), root `package.json` version `2.0.29`. Paths below are relative to that repository. "Inferred" marks conclusions not read directly from code.

**Old and new architecture.** This tree is the finished **V2 rewrite** and contains no V1 runtime. `packages/opencode` (the V1 Hono server, AI-SDK `streamText` loop, and `Storage` JSON files) no longer exists. V1 remains in three places:
- `packages/core/src/v1/config/*` migrates V1 configuration.
- `packages/core/src/database/v1-migration.bun.ts` imports V1 data and renames tools: `bash→shell`, `task→subagent`, `apply_patch→patch` (`:899`).
- Stale docs such as `perf/test-suite.md`, which still says "Run from `packages/opencode`".

`CHANGELOG.md` opens with "Release notes for OpenCode V2". `V2_HTTP_API_AUDIT.md` is a completed, endpoint-by-endpoint review of the 139 operations in `packages/protocol/openapi.json`, each marked Keep, Change, Remove or Experimental-only. `specs/v2/*` holds the current contracts (`session.md`, `tools.md`) and decision records (`event-stream-architecture.md`, `provider-policy.md`). Where these specs differ from code, the code is cited.

---

## 1. Identity

- **Language and runtime.**
  - TypeScript throughout, built heavily on **Effect v4**: catalog `effect`/`@effect/*` 4.0.1, `Context.Service`, `Layer`, `Schema`, `effect/http-api`.
  - Bun 1.4.2 is the package manager and primary runtime.
  - Core also has Node and **workerd** (Cloudflare) runtime variants, selected through conditional `imports`: `#sqlite`, `#pty`, `#shell-parser-wasm` (`packages/core/package.json`; `packages/server/src/workerd.ts` boots core inside a Durable Object).
- **License:** MIT (`LICENSE`).
- **Distribution.**
  - `@opencode/cli` ships `bin/opencode.cjs`, a Node shim that spawns a Bun-compiled single executable per platform (`packages/cli/script/build.ts`, `packages/cli/bin/opencode.cjs`).
  - Install routes: `curl opencode.ai/install`, npm, and Homebrew (`anomalyco/tap`); README lines 50-57.
  - Desktop app: Electron, packaged with `electron-builder` (`packages/desktop`).
  - Also shipped: a VS Code extension (`sdks/vscode`), a GitHub Action (`github/action.yml`), and Nix (`flake.nix`).
- **Packaging.** Bun workspaces (`packages/*`, `packages/plugins/*`, `services/*`, `packages/console/*`, `packages/stats/*`) with Turbo for typecheck. The repository URL in package manifests is `anomalyco/opencode`.
- **Size.** About 53 workspace packages. Roughly 890k lines of `.ts`/`.tsx` in total, about 590k excluding tests and generated files.
  - Largest packages: `app` (~178k, web GUI), `core` (~173k), `tui` (~107k), `ai` (~80k), `gui-extensions` (~61k).
  - About 1,070 test files, of which core has 262, tui 171, app 162 and ai 135.

## 2. Repository layout

Dependency direction is stated in `AGENTS.md`: Schema → Core and Protocol → Server. Client depends only on Schema and Protocol. SDK composes Client, Core and Server.

| Package | Role |
|---|---|
| `packages/schema` | Effect Schemas for public domain shapes and **durable event payloads**, such as `session-event.ts` and `session-message.ts`. |
| `packages/core` | Runtime: sessions, runner, tools, permissions, config, plugins, MCP, snapshots, SQLite and Drizzle, PTY, shell parsing. |
| `packages/ai` | OpenCode's **own LLM client library**: about 45 wire protocols (`src/protocols/*`), about 60 provider definitions (`src/providers/*`), routing and transports (HTTP and WebSocket), cache policy, plus media (image, speech, video, transcription). |
| `packages/protocol` | `effect/http-api` endpoint definitions grouped by domain (`src/groups/*`). Generates `openapi.json`. |
| `packages/server` | Assembles the `HttpApi`. Adds auth, CORS, the SSE event feed (`event-feed.ts`), pairing, and a workerd profile. |
| `packages/client` | **Generated** Promise and Effect clients (`src/promise/generated`), built by `packages/httpapi-codegen`. |
| `packages/sdk` | In-process host: runs the Server router in memory with no listener (`packages/sdk/README.md`). |
| `packages/cli` | `opencode` binary: command router, managed background service, `run`, `serve`, `acp`, `mcp`, `auth`, `session`, `plugin`. |
| `packages/tui` | Terminal UI built on **OpenTUI and Solid**. |
| `packages/app`, `session-ui`, `ui`, `gui-extensions` | SolidJS web GUI and shared components, used by both web and desktop. |
| `packages/desktop` | Electron shell. |
| `packages/codemode` | A pure, confined interpreter for a JavaScript-like language used for "Code Mode" tool calling. |
| `packages/plugin`, `plugin-browser`, `plugins/figma` | Plugin API (Effect and Promise flavours), a browser plugin, and a Figma MCP and skills plugin. |
| `packages/http-recorder`, `simulation` | Cassette record and replay for HTTP and WebSocket, and a simulated provider with TUI driver. |
| `packages/console`, `enterprise`, `stats`, `function`, `web` | Hosted SaaS console, enterprise app, stats site, cloud functions (including the GitHub app), and Astro docs. |
| `github/` | GitHub Action (`/opencode` comments). |

## 3. Process topology

- **Client/server split is total.** Every UI (TUI, web, desktop, ACP, `run`) talks to a server over the generated HTTP client. The single exception is the embedded SDK, which runs the same router in memory.
- **Managed background service.** By default the CLI connects to a per-user, **elected** background server (`opencode service start|status|stop|get password`).
  - Connection resolution is in `packages/cli/src/services/server-connection.ts`:
    - `--server <url>` connects to an explicit server and only warns on a version mismatch.
    - `--standalone`, or a disabled service, spawns a private child `opencode serve --stdio --port 0` with a random password. It treats **stdin EOF as the end of the lease** (`packages/cli/src/services/standalone.ts`).
    - Otherwise it ensures or replaces the managed service.
  - Auth is HTTP basic `opencode:<password>`.
  - Multiple TUIs and windows share one server, and one server serves many directories ("Locations").
- **Transport.**
  - REST endpoints live under `/api/*` (`packages/protocol/src/groups/session.ts`; for example `POST /api/session/:id/prompt`, `/interrupt`, `/compact`, `/revert/stage|commit`, `/inbox`).
  - One global **SSE** stream at `GET /api/event` (`protocol/src/groups/event.ts:44`). The server encodes each event once and fans it out to **independent bounded per-connection queues** of 4,096 frames (`server/src/event-feed.ts:8,76`). A slow subscriber is disconnected without blocking healthy ones (`specs/v2/event-stream-architecture.md`).
  - A durable, replayable per-session log at `GET /api/experimental/session/:id/log?after&follow`.
  - Plugin RPC at `POST /api/rpc/:rpcID/:method` (`protocol/src/groups/rpc.ts`).
  - PTY and persistent-terminal endpoints.
- **Clients.** TUI, web app, Electron desktop, and the ACP agent (`opencode acp`, `packages/cli/src/acp/*`, using `@agentclientprotocol/sdk`). Also the VS Code extension and the GitHub Action. Remote device pairing goes through OpenTunnel (`cli/src/services/remote-tunnel.ts`, `server/src/pairing.ts`).
- **Execution placement.**
  - `SessionExecution` is process-global and keyed by Session ID.
  - The runner, tools, permissions and filesystem are **Location-scoped**: one set of services per directory, obtained from `LocationServiceMap`.
  - Different sessions run concurrently. Wakes for the same session coalesce (`core/src/session/run-coordinator.ts`, `specs/v2/session.md`).

## 4. Agent loop

The loop is not an in-memory tool loop. **Each step is one `llm.stream` call that continues from history reloaded from SQLite** (`AGENTS.md` "Session Core"; `specs/v2/session.md`).

1. **Admission.** `Session.prompt` publishes the durable `session.inbox.enqueued` event. Its projection inserts a `session_inbox` row. The prompt then schedules an advisory `SessionExecution.wake(sessionID)` (`core/src/session/inbox.ts`, `session.ts`). Delivery modes are `steer` (the default: injected at the next safe step boundary) and `queue` (delivered when idle). Compaction and "move session" requests are control items in the same inbox.
2. **Drain.** `SessionRunner.drain` (`core/src/session/runner/llm.ts:57`):
   - Awaits plugin activation.
   - Settles stale compactions and orphaned tool calls left by a crashed process, marking them failed with "Tool execution interrupted" (`:332-385`).
   - Loops `advanceToStep` (`:74`). This promotes inbox items (`SessionInbox.promote`), handles manual compaction and move controls, and starts title generation in a background fiber (`:177`).
3. **Step.** `runStep` (`llm.ts:209`):
   - Prepares instruction deltas.
   - Runs **automatic compaction** if the estimate exceeds budget (`:220`).
   - Builds the transcript.
   - Calls `context.request.primary` (`:234`), which runs session `context` hooks and selects tools.
   - Calls `SessionStep.attempt`.
4. **Attempt.** `SessionStep.attempt` (`core/src/session/runner/step.ts:73`):
   - Captures a filesystem snapshot concurrently with the request (`:76`).
   - Streams `llm.stream(request)` (`:107`) and publishes every LLM event as session events through `createLLMEventPublisher` (`runner/publish-llm-event.ts`).
   - **Tool dispatch is eager and parallel.** As soon as a complete `tool-call` event arrives (and is not provider-executed), its execution is forked into its own fiber (`:122-134`), before the stream finishes. Each call publishes `Tool.Called` durably *before* side effects begin. Outcome publication is serialized.
   - Every tool output passes through `toolOutput.truncate` (`:127`).
   - After the stream ends, the attempt joins all tool fibers and classifies user declines, interrupts and defects (`:298-321`).
   - It takes the end snapshot and the list of changed files, and publishes `session.step.ended` with usage, cost, snapshot and files (`:234-261`).
5. **Continuation.** The step returns `needsContinuation`, true when there were local tool results. The drain loop then increments `step` and runs another step from reloaded history (`llm.ts:191-198`). The loop ends when no continuation is needed and no inbox work is promotable.

**Stop conditions and step limits.**
- An agent's `steps` field sets a limit. When it is reached, the final step appends `MAX_STEPS_PROMPT` as an assistant message and sets `toolChoice: "none"`. Tool definitions are kept so the provider's cached prefix survives (`llm.ts:226-244`; `runner/max-steps.ts`).
- New user input resets the allowance.

**Outcomes and retries** (`step.ts:32-43`, `llm.ts:273-327`):
- `Retry`: generic backoff. Up to 10 retries, exponential from 2 s capped at 10 s per gap, jittered, honouring `retry-after` up to 15 minutes. Timeouts are capped at 3 retries (`runner/retry.ts:40-60`).
  - **Discrepancy:** `specs/v2/session.md` says "initial request plus at most four retries".
  - A plugin `retry` hook can override the decision.
- `Continue`: an incomplete stream after durable output. The partial assistant message is kept, a synthetic "continue where you left off" message is added, and a new assistant message starts (`llm.ts:35, 281-289`).
- `OutputLimit`: `finish=length`. The runner auto-continues with a synthetic nudge and fails after three consecutive length-truncated responses (`llm.ts:290-319`).
- `Compacted`: the provider reports context overflow before any output. The runner performs one overflow compaction and rebuilds the step (`step.ts:156-167`).
- `RecoverFull`: a transport error asks for a full-context rebuild, for example after rejected provider continuation state (`step.ts:182-188`).
- A content-filter finish fails the step.

**Interruption** uses Effect fiber interruption. `POST /interrupt` stops the locally owned chain. Pending inbox items survive.

**Crash recovery.** A write-ahead execution claim is persisted when a busy period starts. At startup, top-level sessions that still hold a claim are resumed with bounded attempts (`core/src/session/execution/restart.ts`; `specs/v2/session.md`). Each resume adds a "The server restarted while you were working" message and continues the turn without user input; a turn resumed more than 10 times is failed instead. Tools left running are marked failed with "Tool execution interrupted", never re-run. The spec states this gives no exactly-once guarantee. See [Crash Recovery](../crash-recovery/README.md#opencode).

## 5. Model and provider layer

- **Own SDK.** `@opencode/ai` (`packages/ai`) replaces the Vercel AI SDK on the hot path. It provides a normalized `LLMRequest` and `LLMEvent` stream, the `LLMClient` service, and per-protocol body builders and stream parsers. Examples: `protocols/anthropic-messages.ts` (1,676 LOC), `openai-responses.ts`, `gemini.ts`, `bedrock-converse.ts`, and `open-responses-continuation.ts`.
- Transports are HTTP SSE and **WebSocket channels**. The runner passes `webSocket: "session"` (`llm.ts:245`) for OpenAI-Responses-style persistent sessions (`ai/src/route/transport/websocket-channel.ts`).
- **AI SDK compatibility.**
  - `core/src/aisdk-native.ts:59-80` maps `@ai-sdk/*` package names in user config onto native `@opencode/ai/providers/*` routes.
  - Unmapped `@ai-sdk/*` packages still load through an adapter (`core/src/aisdk.ts`, which adapts `LanguageModelV3` streams into `LLMEvent`) by way of `model-resolver.ts:204`.
- **Catalog.** models.dev (`core/src/models-dev.ts`) with a bundled snapshot (`models-dev/snapshot.txt`). The catalog supplies costs (including tiered), limits, modalities, reasoning options and `interleaved` flags.
- **Providers.** Provider definitions live in `ai/src/providers/*`: Anthropic, OpenAI, Azure, Bedrock (plus Mantle), Google, Vertex (chat, messages, responses, mistral), xAI, Groq, Mistral, Cohere, DeepSeek, DeepInfra, Fireworks, Together, Cerebras, OpenRouter, Vercel and Cloudflare gateways, Moonshot, Z.ai, MiniMax, Alibaba, Meta, Venice, Baseten, DigitalOcean, and `opencode` (Zen).
  - Auth and discovery plugins live in `core/src/plugin/provider/*`: GitHub Copilot, ChatGPT subscription, GitLab, Databricks, Snowflake Cortex, SAP AI Core, Ollama, LM Studio, vLLM, NVIDIA, Poe, and others.
  - Credentials are stored in SQLite (`credential/sql.ts`), with multi-account switching (CHANGELOG 2.0.29).
- **Policy.** `policies` statements (`provider.use`, `tool.use`, `integration.use`, last match wins) can deny a provider. Console- or MDM-managed statements have final authority (`specs/v2/provider-policy.md`; `config.ts:246-290`).
- **Caching.**
  - `ai/src/cache-policy.ts` uses the default `"auto"` policy. It places up to 4 breakpoints: last tool, first and last system parts, and the conversation tail. It applies only to protocols that honour inline hints.
  - A provider plugin sets the OpenAI prompt cache key (`plugin/provider/prompt-cache-key.ts`).
  - The **warming plugin** sends a keep-alive "Reply with exactly: OK" request every 4 minutes for up to 30 minutes to keep the cache warm. It is opt-in through the `warming` config (`core/src/plugin/warming.ts:11`).
- **Reasoning.**
  - `core/src/variant.ts` derives model "variants" (effort levels, on/off toggle, `budget_tokens`; Anthropic adaptive thinking). They are addressed as `provider/model#variant`.
  - Reasoning parts are stored and replayed with provider metadata (signatures, encrypted content) **only to the producing model**. After a model switch they are degraded to text (`session/runner/to-llm-message.ts:157-213`).
  - The `VerbosityPlugin` sets OpenAI `textVerbosity`.

## 6. Tool system

**Definition.** `Tool.make({ description, input, output?, execute })` (`specs/v2/tools.md`).
- `input` and `output` are Effect Schema or Standard Schema with JSON Schema; MCP tools use runtime JSON Schema.
- A tool returns up to three values:
  - `output`: a typed machine value consumed by Code Mode.
  - `content`: model-visible and durably stored.
  - `metadata`: compact JSON for the UI.
- The invocation context is `{sessionID, agent, messageID, callID, progress}`.

**Registration.**
- Registration goes through the Location-scoped `Tool.Service.transform` editor (`core/src/tool.ts`; `core/src/tool/AGENTS.md`). Registrations are scoped: the latest active one wins, and disposing it reveals the previous one.
- Each model request **captures a snapshot** of definitions and executors.
- Names are normalized, and the name `execute` is reserved (`tool.ts:313`).

**Validation and repair.**
- The registry decodes input before execution and validates output after it.
- `ToolInputRepairPlugin` (`plugin/tool-input-repair.ts`) runs an `execute.before` hook. It repairs common model JSON mistakes only when the schema is unambiguous: stringified objects or arrays, `"20"` → 20, null placeholders, and extra keys on closed objects.

**Code Mode.**
- By default MCP tools and `opencode_*` namespace tools are *not* sent as native tools (`codemode` defaults true; `tool/mcp.ts:45`). They are listed in an instruction catalog and reachable only through one `execute` tool. The model writes JavaScript-like code calling `tools.ns.name(...)`, and a `search()` helper is available when the catalog is partial (`core/src/codemode/instructions.ts`; `tool.ts:238-277`).
- The interpreter in `packages/codemode` is a pure interpreter, not a JS sandbox. It offers no network, filesystem or process access except through host tools, and supports limits on time, calls and output.
- MCP servers that are themselves Code Mode servers opt out (`plugin/mcp-codemode-defaults.ts`).

**Built-in tools** (`core/src/tool/plugin/*`, registered in order in `plugin/internal.ts:216-250`):

| Tool | Description |
|---|---|
| `read` | Read a file (line-numbered) or list a directory, including media. After the read it **discovers nested `AGENTS.md`** up to the Location root and injects them as durable session instructions (`read.ts:82-104`). |
| `write` | Create or overwrite a file. Permission action `edit`. |
| `edit` | Exact `oldString`→`newString` replacement, optionally `replaceAll`. See the edit strategy below. |
| `patch` | Codex-style `*** Begin Patch` envelope with Add, Delete, Update and Move (`tool/patch.txt`). |
| `glob`, `grep` | File search through ripgrep and the filesystem service. Default limits apply. |
| `shell` | Run a command, foreground or `background: true` (as a Job, with a synthetic completion notification). Uses timeouts, sets env `AGENT=1 OPENCODE=1`, and parses the command for permissions (see §9). |
| `subagent` | Spawn or continue a child session with an agent and optional model or variant; foreground or background (`subagent.ts`). |
| `question` | Ask the user structured multiple-choice questions through a "form". |
| `skill` | Load a skill's content. |
| `webfetch`, `websearch` | Fetch a URL as markdown, text or HTML. Search uses a pluggable provider: Exa, Tavily, Firecrawl, Parallel, TinyFish or MCP (`plugin/websearch/*`). |
| `opencode_*` (Code Mode) | `session_rename`, `session_move`, `worktree_create`, `worktree_list`, `worktree_remove`, `models` (search the model catalog) (`tool/plugin/opencode.ts`). |
| MCP resource tools | List and read MCP resources (`mcp-resource.ts`). |
| Browser plugin | Desktop browser control (`@opencode/plugin-browser`). |

There is no todo or todowrite tool in V2. A grep of `core/src/tool` and `core/src/plugin` found none.

**Per-model tool selection.** The patch plugin hook gives `gpt-*` models (excluding `oss` and `gpt-4`) `patch` and removes `edit` and `write`. All other models get `edit` and `write` without `patch` (`tool/plugin/patch.ts:299-305`). Tool removal for Claude and GPT grep and glob exists but is disabled (`plugin/optimize.ts:31-49`).

**Edit strategy** (`tool/plugin/edit.ts`):
- Matching runs against the **LF view** that Read showed the model. Offsets are then mapped back to the CRLF source.
- Match tiers are tried in order:
  1. Exact match.
  2. Match after Unicode normalization of typographic quotes, dashes and spaces.
  3. Line-wise match ignoring trailing whitespace (`matchView`, `:166-190`).
- More than one match fails unless `replaceAll` is set.
- Replacement lines reuse the original line endings via an LCS alignment, so mixed-ending files keep their bytes (`withRegionEndings`).
- The BOM is preserved.
- The permission prompt carries a diff preview.
- Files are written through `FileMutation`, which runs the formatter.

**Output truncation.**
- The default is 2,000 lines or 50 KiB, with a head-plus-tail cut (`core/src/tool-output.ts:4-7`).
- The full text is spilled to `<data>/tool-output/tool_*`, kept for 7 days, and replaced with a preview. If spilling fails, the call fails rather than publishing lossy success.
- Limits are configurable through the `tool-output` config plugin.

**LSP and diagnostics.** **Not implemented in V2.** Configuration still parses `lsp` (`config/normalize.ts:492`), but `edit.ts`, `write.ts` and `file-mutation.ts:131` carry `TODO: Notify LSP and collect diagnostics after LSP runtime exists`. This is a regression from V1, which ran LSP diagnostics after edits (inferred from memory of V1, not verifiable in this tree). **Formatters** do run after mutations, with a built-in list including gofmt, prettier or biome, ruff, rubocop, zig, ktlint, dart, terraform and others (`core/src/formatter/builtins.ts`).

## 7. Context engineering

**System prompt.**
- The base prompt is a short (756-byte) template, `session/runner/prompt/system.txt`. Tool guidance is rendered into it depending on which of `shell`, `write` and `edit` exist (`session/system-prompt.ts`).
- **Per-model prompts** are plugins (`plugin/optimize.ts`):
  - `gpt.txt` or `gpt-astra.txt` override for GPT.
  - `anthropic.txt` is *appended* for Claude.
  - `kimi.txt`, `trinity.txt` (Arcee) and `meta.txt` override for their models.
  - The same hook runs for the `context`, `compaction` and `generate` paths.
  - An agent with its own `system` prompt skips the override.
- `IdentityPlugin` inserts a "# Your Model" block with the name, provider ID and model ID (`plugin/identity.ts`).

**Instructions as durable value deltas** (`specs/v2/session.md`, "Instructions Are Value Deltas"; `session/instruction-state.ts`):
- Sources are built-ins, ambient discovery (AGENTS.md), skill list, references, MCP guidance, and API-managed entries.
- Before each attempt all sources are read and hashed. Changes are admitted as one `session.instructions.updated {delta, text?}` event, with bodies stored once in `instruction_blob`.
- The epoch baseline renders into the system prompt. **Later changes are appended as chronological system messages** rather than rewriting the prefix, which keeps the prompt cache valid. The skill list and Code Mode catalog each provide `update()` diff renderers (`skill/instructions.ts`, `codemode/instructions.ts`).
- Plan mode is handled the same way, with an enter or leave `<system-reminder>` appended near the tail (`plugin/plan.ts`).

**Instruction files.**
- **Only `AGENTS.md`.** The files read are the global `~/.config/opencode/AGENTS.md` plus an upward walk from the Location to the project root (or home), watched for changes (`config/plugin/instruction.ts:38-72`).
- Nested `AGENTS.md` are discovered lazily by `read` (§6).
- `.claude/` and `.agents/` directories *are* scanned as config roots (`config/discovery.ts:85`), so skills, agents and commands from those directories load. `CLAUDE.md` is not read; a grep of core found no reference.
- `/init` is a built-in "guided AGENTS.md setup" command (`plugin/command.ts:30`).

**Compaction** (`session/compaction.ts`, 963 LOC):
- **Trigger:** before each step, the full request is estimated against context minus reserved headroom (minimum 16k, or 10% free by default). Auto-compaction can be disabled.
- **Output:** a structured rolling summary with fixed headings: Objective, Requirements, Decisions, Work State (Completed, Active, Blocked), Next Move, Relevant Files (at most 15), Important Context (`:104-145`). About 15k tokens of recent conversation are kept verbatim (`keep: 15_000`, `:197`).
- **Updates** are incremental: the previous checkpoint is merged with newer history.
- **Overflow handling:** if the provider rejects the request as too long, retries shrink to 70%, 50% and 35% (`SHRINK_STEPS`, `:95`), and old tool outputs are flattened and truncated to 1,250 characters for the summarizer.
- **Native compaction:** the result can be `providerContext` (provider-hosted compaction) instead of a text summary (`:71`).
- **History:** the full transcript remains durable, and only active history is rebuilt. A `compaction` session hook lets plugins supply the summary.

**Memory.** No long-term memory feature was found in V2. **Skills** are discovered from `skill/` and `skills/` under every config directory (`.opencode`, `.claude`, `.agents`, global) and from URLs (`config/plugin/skill.ts:77-100`). They are advertised as an `<available_skills>` XML list and loaded with the `skill` tool. Users can also invoke a skill directly (`POST .../skill`).

## 8. Sessions and state

**Storage.**
- One **SQLite** database through Drizzle: `<XDG data>/opencode/opencode.db`. Non-default release channels use `opencode-<channel>.db`; `OPENCODE_DB` overrides the path (`packages/cli/src/database-path.ts`).
- Bun, Node and workerd (Durable Object) SQLite drivers are all supported.
- File permissions are restricted to the owner (`database/database.ts:85`).

**Event-sourced.**
- Each durable event commits in one SQLite transaction that runs the projectors and advances `event_sequence` (`core/src/bus.ts:315-433`). The raw event row is also written to the `event` table, keyed by `(aggregate_id, seq)` (`core/src/event/sql.ts`), only when `events.persist` is on. It is off by default on the Node server and forced on in workerd (`bus.ts:203`, `server/src/routes.ts:119`, `server/src/workerd.ts:69`).
- **Projectors** (`session/projector.ts`, 810 LOC) fold events transactionally into read models: `session`, `session_message`, `session_inbox`, `session_pending`, `instruction_*` (`session/sql.ts`).
- `AGENTS.md` sets the rule: "record irreducible new facts … Enrich projections."
- Live-only deltas (text, reasoning, tool-input) are not replayed.

**Message model** (`schema/src/session-message.ts`):
- Message types: `user`, `synthetic`, `system`, `skill`, `shell`, `assistant`, `compaction`, plus switch markers (`agent-switched`, `model-switched`, `location-switched`).
- Assistant content items are `text`, `reasoning` and `tool`. Tool state runs `streaming → running → completed|error`.
- Event types include `session.step.started/streamed/ended/failed`, `session.tool.called/progress/success/failed`, `session.retry.scheduled`, `session.compaction.*`, `session.revert.*` and `session.inbox.*` (`schema/src/session-event.ts`).

**Resume and fork.**
- Sessions resume from the projected history, and recovery covers process death (§4).
- `POST /session/:id/fork` copies messages up to a boundary and adopts the parent's latest instruction values.
- Import and export are experimental endpoints.
- **Share is gone in V2.** No share endpoint appears in `protocol/src/groups`.
- Sessions can **move** between Locations, such as worktrees, through the `session_move` tool or API.

**Snapshots and undo.**
- Snapshots are **git-based shadow repositories** at `<data>/snapshot/<projectID>/<hash(worktree)>`, seeded from the project repo (`core/src/snapshot.ts:95-96`). They record content-addressed trees through `git tree capture` and are captured at step start and end. Each `step.ended` records the snapshot and the changed files.
- Revert is a staged flow (`/revert/stage` → `/revert/commit` or `DELETE /revert`) that restores selected paths (`session/revert.ts`).
- Snapshots are enabled only when the VCS is git (`snapshot.ts` `enabled()`); a Mercurial VCS plugin exists for other VCS features.

## 9. Permissions, safety, sandboxing

**Rules.** Rules are `{action, resource, effect: allow|ask|deny}` with wildcards. **The last matching rule wins**, and the default is `ask` (`core/src/permission.ts:87-96`).
- The default agent ruleset is **allow `*`**, except reads of `*.env` and `*.env.*`, which ask (`*.env.example` is allowed) (`schema/src/agent.ts:46-51`).
- In practice most tools run without prompting by default. Exceptions are external directories and `.env` reads (`file-access.ts:117`; inferred from the rule set).

**Tools own their permission requests.** Each tool calls `permission.assert` itself with action, resources, `save` patterns and metadata such as a diff preview. The registry performs no authorization (`specs/v2/tools.md`; `tool/AGENTS.md`).
- Reply options are once, always (saved to a table, `permission/saved.ts`) and decline.
- A decline with feedback (`CorrectedError`) becomes a model-visible failure, so the model continues.
- A plain decline interrupts the step.

**Shell.** The command is parsed with **tree-sitter WASM** (bash or PowerShell) to extract per-command resources, save patterns and referenced directories (`tool/plugin/shell.ts:116-140`; `core/src/shell/parse.ts`, `scan.ts`). Directories outside the Location trigger an `external_directory` approval.

**Agents and policy.**
- The Plan agent denies `edit` except under `~/.opencode/plan/*`.
- Explore denies everything except read-only tools (`plugin/agent.ts`, `plugin/plan.ts`).
- Organization `policies` (`tool.use` deny) override user configuration and cannot be removed by repository configuration (`plugin/internal.ts` `guarded`).

**Sandboxing.** **No OS-level sandbox** was found: no seatbelt, bwrap or landlock. Core files mentioning "sandbox" refer to workspace or provider sandboxes and MCP stdio. The Code Mode interpreter is a language-level confinement for tool orchestration only. Safety relies on permissions plus git snapshots.

## 10. Extensibility

**Plugins everywhere.** Even core behaviour is built from internal plugins in an ordered list: the `pre` list (tools, prompts, agents, providers, MCP, VCS) and the `post` list (config-driven loaders for instructions, agents, commands, compaction, formatter, snapshot, skills, policy) (`core/src/plugin/internal.ts:216-268`).
- External plugins use `@opencode/plugin` (Promise API) or `@opencode/plugin/effect`. They are loaded from npm or local paths, and the `opencode plugin add/check/update/remove` commands manage them.
- The plugin context exposes domains: agent, aisdk, command, credential, event, integration, mcp, model, generate, permission, provider, reference, rpc, session, shell, skill, storage, tool, vcs, websearch, worktree (`plugin/src/effect/plugin.ts:27-54`).
- Two mechanisms:
  - **Transforms:** synchronous editors over stateful catalogs, replayed on reload.
  - **Hooks:** the `session` domain has prompt, context, compaction, generate, title, model.request, http.request and http.response, experimental WebSocket handshake/send/receive, and retry. The `tool` domain has `execute.before` and `execute.after`. There are also permission, shell and aisdk domains (`core/src/plugin/hooks.ts:13-19`; `plugin/src/effect/session.ts:138-151`).
- Plugins can expose **RPC** to clients (`/api/rpc`) and TUI extensions (`plugin/src/tui`; `tui/src/feature-plugins`).

**Event bus.** `Bus` publishes typed durable and live events. Plugins `ctx.event.subscribe()`.

**MCP.** Uses the official `@modelcontextprotocol/client` 2.0, over stdio and HTTP with OAuth (`core/src/mcp/*`). MCP prompts become slash commands (`plugin/command.ts:57`), resources are exposed via tools, server instructions are injected, and tools go through Code Mode by default.

**Agents and modes.**
- Built-in agents: `build` (default), `plan`, `general` and `explore` (subagents), and hidden `compaction`, `title` and `summary` (`plugin/agent.ts`).
- Custom agents come from config `agents` or from `{agent,agents,mode,modes}/**/*.md` under config directories (`config/plugin/agent.ts:22-26`). Each has its own prompt, permissions, `steps`, model and request overlays.

**Subagents.** The `subagent` tool creates child sessions. They run in the foreground or the background (`SubagentJob`, with completion delivered as a synthetic message), can be continued by `sessionID`, and can override the model or variant.

**Commands.** Built-ins are `/init` and `/review`. Markdown commands come from config directories, and MCP prompts also become commands.

**Formatters, references, worktrees.** Formatters are config-driven with built-ins. References are configurable external context (`config/plugin/reference.ts`). Worktrees use pluggable strategies (`worktree/strategies.ts`).

## 11. Clients and UI

- **TUI.** `packages/tui` uses **OpenTUI** (`@opentui/core` 0.5.17, Zig-backed) with **SolidJS** reactivity. It has a story or fixture system (`feature-plugins/system/storybook`), a theme-token system, a "mini" inline mode (`opencode mini`), attention sounds, and terminal program-status notifications. It talks only through `@opencode/client` plus the SSE feed.
- **Web and desktop.** `packages/app` is a SolidJS single-page app (Solid Router, TanStack Query and Virtual) served by the CLI (`cli/src/services/web-ui.ts`). The Electron desktop app (`packages/desktop`) reuses it through `gui-extensions` and `session-ui`.
- **SDK generation.** `effect/http-api` definitions produce `protocol/openapi.json`, from which `httpapi-codegen` generates Promise and Effect clients (`packages/client`, `bun run generate`). `@opencode/sdk` embeds the full host in-process.
- **GitHub.** `github/action.yml` responds to `/opencode` comments by explaining issues, fixing them, or opening PRs. `packages/function/src/github.ts` is the hosted app backend.
- **ACP.** `opencode acp` implements the Agent Client Protocol on top of the client: sessions, permissions, elicitation, replay and tool translation (`packages/cli/src/acp/*`).
- **Other surfaces.** A VS Code extension (`sdks/vscode`), an Astro and Starlight docs site (`packages/web`), and a hosted console (`packages/console`) that can deliver organization policy.

## 12. Configuration

**Files.** `opencode.json` / `opencode.jsonc` (`config/discovery.ts:12`), plus `.opencode/` directories that hold agents, commands, skills, plugins and config. `.claude/` and `.agents/` directories are also config roots for skills, agents and commands.

**Precedence**, low to high (`core/src/config.ts:246-290`):
1. Managed policies (loaded first, so they evaluate last).
2. Well-known remote config (`wellknown`).
3. Global `~/.config/opencode/`.
4. Explicit `OPENCODE_CONFIG` file.
5. Direct project files found by an upward walk.
6. Project `.opencode/` directories.
7. `OPENCODE_CONFIG_CONTENT` inline config.
8. **Managed settings last**: system directories (`/etc/opencode`, `/Library/Application Support/opencode`, `%ProgramData%\opencode`) and macOS MDM `ai.opencode.managed.plist`.

**Schema.** Effect Schema in `packages/schema/src/config/*`. Normalization reports diagnostics (`config/normalize.ts`). Variable substitution is supported (`config/variable.ts`). Configuration is watched and hot-reloaded: plugin transforms are replayed per Location.

**Migration.** V1 configuration migrates automatically (`core/src/v1/config/migrate.ts`, for example `write`/`patch` → `edit` permission).

**Environment variables.** Notable ones are `OPENCODE_DB`, `OPENCODE_CONFIG_DIR`, `OPENCODE_PASSWORD` and `OTEL_EXPORTER_OTLP_ENDPOINT` (OpenTelemetry built in; `cli/src/index.ts`).

## 13. Testing and quality infrastructure

- **Test runner.** `bun test` per package; tests are refused from the repository root. About 1,070 test files.
- **Recorded tests.** `@opencode/http-recorder` provides deterministic HTTP and WebSocket cassettes. `packages/ai/test/*.recorded.test.ts` replays recorded provider traffic, with about 274 JSON fixtures, including Anthropic cache, golden tests and continuation scenarios.
- **Simulation.** `packages/simulation` has a simulated OpenAI-compatible backend and a TUI frontend driver (actions, renderer, semantics) for end-to-end UI scenarios.
- **Lint.**
  - `oxlint`.
  - **ast-grep rule packs** for Effect patterns and simplifications, with their own rule tests (`script/ast-grep/*`; root `package.json` `lint:effect-*`, `test:*-rules`).
  - `tsgo` (TypeScript native preview) typecheck through Turbo.
- **Process docs.** Detailed style rules in `AGENTS.md`: no `else`, no destructuring, no aliased or star imports, prefer mocks only as a last resort. TUI stories and Storybook (`packages/storybook`) round out UI testing.
- **CI and benchmarks.** CI workflows include `test.yml`, `check.yml`, `review.yml`, `triage.yml` and `duplicate-issues.yml`; some of these are themselves agent-driven (inferred from their names). Benchmarks: `bench:devex`, `core/script/benchmark-location*.ts`.

## 14. What is distinctive

1. **Event-sourced, durable agent loop.**
   - Every step is one LLM call continued from SQLite-projected history. There is no in-memory tool loop.
   - Prompts pass through a durable **inbox** with `steer` and `queue` delivery.
   - Write-ahead execution claims allow crash recovery, and orphaned tool calls are reconciled at drain start (`runner/llm.ts`, `specs/v2/session.md`).
2. **One shared, elected background server per user** serves many directories ("Locations") and many concurrent clients. Clients include the TUI, web app, Electron app, ACP, embedded SDK, and remote paired devices via OpenTunnel. Sessions can **move** between Locations and worktrees mid-flight.
3. **Its own multi-protocol LLM library** (`@opencode/ai`, about 45 protocols) replaces the Vercel AI SDK. AI SDK packages remain only as a compatibility path. It includes a WebSocket session transport and native provider compaction.
4. **Code Mode by default for MCP and auxiliary tools.** Instead of advertising every MCP tool, the model gets a catalog and one `execute` tool. Calls are written in a custom pure interpreter (`packages/codemode`) with `search()` for large catalogs.
5. **Cache-preserving context design.**
   - Instruction, skill and catalog changes are durable *deltas* appended as chronological system messages.
   - The final step keeps tool definitions with `toolChoice:none`.
   - The plan-mode reminder is appended near the tail.
   - Automatic 4-breakpoint cache placement.
   - An optional cache-**warming** keep-alive ping.
6. **Eager parallel tool execution while the stream is still arriving.** Each completed tool call forks immediately, with durable `Tool.Called` before side effects (`step.ts:122-134`).
7. **Per-model adaptation as plugins.** Model-specific system prompts (GPT, Claude, Kimi, Arcee, Meta) and model-specific edit tools: GPT gets the Codex-style `patch`; other models get `edit` and `write`.
8. **Robust edit matching** that works on the LF view, applies tiered Unicode-quote and trailing-whitespace fallbacks, and preserves per-line endings with LCS.
9. **Tree-sitter shell parsing** for permission resources and directory detection, plus schema-guided **tool-input repair**.
10. **Enterprise policy layer.** Last-match-wins `policies` over providers, tools and integrations. MDM plist and system-directory managed config takes final precedence. Policy plugins are guarded against removal by repository configuration.
11. **Effect-native everything**: services, layers, schemas, HTTP API, and generated Promise and Effect clients. Core even runs on Cloudflare workerd in a Durable Object.
12. **Structured, incremental compaction checkpoints** with a fixed template, verbatim recent tail, shrink-on-overflow schedule, and legacy-format rewrite.

## 15. Strengths, weaknesses, lessons

**Strengths**
- Durability and observability are first-class. Every model-visible fact is a durable event, with replayable per-session logs, crash recovery and per-step snapshots. Multi-client consistency comes from projections plus one SSE feed with per-connection back-pressure isolation.
- A clean contract for tools: one `Tool.make`, a three-part result (output, content, metadata), scoped registration with request snapshots, and generic bounded output with spill-to-disk.
- Context management is careful about prompt caching. This is a real cost and latency lever for long agent runs.
- Broad provider coverage without depending on a third-party SDK's release cadence. Recorded-cassette tests make protocol work safe.
- The specs and `AGENTS.md` encode architectural invariants, such as "no in-memory tool loop" and "durable events minimal".

**Weaknesses and risks**
- Very large and complex: about 590k non-test LOC, Effect v4 throughout, and many layers (Schema, Core, Protocol, Server, Client, SDK). The barrier to contribution is high.
- **V2 feature gaps compared with V1:** no LSP diagnostics (TODOs in `edit.ts`, `write.ts`, `file-mutation.ts`), no session share, and no todo tool. Docs drift: `perf/test-suite.md` refers to `packages/opencode`, and the retry spec says 4 retries while the code allows 10.
- Permissive defaults (allow `*`) and no OS sandbox. Safety rests on git snapshots and revert.
- Code Mode adds an interpreter the model must learn. Tool-call errors inside code may be harder for models to recover from (inferred).
- Model-family detection uses string matching on IDs (`id.includes("gpt")`, `"claude"`, `"kimi"`), which is brittle.

**Lessons for building your own harness**
1. Make the loop restartable: persist each step and every tool call *before* side effects, then rebuild the next request from storage. Crash recovery, multi-client views and remote control then come almost for free.
2. Separate admission (an inbox) from execution (a drain). This gives steering, queueing, idempotent retries and compaction-as-a-control-item cleanly.
3. Treat prompt-cache stability as a design constraint. Append deltas instead of rewriting the system prompt, and keep tool definitions stable even when disabling tools.
4. Let tools own their permission requests with structured resources. Parse shell commands rather than regex-matching them.
5. Bound tool output in one generic place, spill the full text to disk, and give the model the path.
6. For large MCP tool surfaces, consider a catalog plus a single code-execution tool instead of hundreds of native tool definitions.
7. Keep tests deterministic with recorded provider cassettes. Write explicit specs for cross-module invariants.
