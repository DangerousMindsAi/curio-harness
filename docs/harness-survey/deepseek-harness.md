# DeepSeek Harness (dsh): architecture report

Snapshot: [DangerousMindsAi/deepseek-harness](https://github.com/DangerousMindsAi/deepseek-harness) at `d743267` (2026-10-09). Paths below are relative to that repository. "Inferred" marks a claim not read directly from code or docs.

## 1. Identity

- **Language/runtime:** TypeScript, ESM-only (`"type": "module"`), Node `^22.19.0 || >=24.0.0` (`package.json` `engines`). pnpm workspace (`pnpm@11.28.5`). The source launch runs through `node --import tsx/esm`; production runs built `lib/` (tsc for declarations plus tsdown bundling; `package.json` `build:lib:*` scripts).
- **Framework:** a vendored, rescoped copy of **Cordis** (`@deepseek-ai/cordis` 4.0.0-rc.7 plus loader, include, group, hmr, timer, schemastery, cosmokit; `vendor/README.md` manifest). Cordis is the dependency-injection and plugin framework behind Koishi (inferred from the upstream `cordiverse/cordis` repository). The harness keeps a local-modifications log, including `cordis/src/fiber.ts` lifecycle hardening (`vendor/README.md` item 6).
- **Distribution:** the npm CLI `@deepseek-ai/dsh` (`apps/cli/package.json` bin `dsh` → `lib/bin.js`), an Electron desktop app (`apps/desktop`, signed macOS/Windows builds), a Python SDK (`deepseek-harness-sdk`) plus a per-platform runtime wheel that packages the same `dsh` CLI (`python/sdk-runtime`), and the native prebuilt package `@deepseek-ai/node-addon-system` (`native/system`).
- **License:** MIT, "Copyright (c) 2026 DeepSeek" (`LICENSE`).
- **Size:** 331 workspace packages under `packages/<group>/<pkg>/` across about 55 groups. That count includes 44 experimental packages and 63 browser `client/` packages. Measured by line count:
  - non-test TypeScript in `packages/*/*/src`: ~418k lines;
  - tests: ~466k lines;
  - `apps/`: ~106k lines;
  - `scripts/` (gates and generators): ~74k lines;
  - Python SDK: ~5k lines;
  - vendored Cordis: ~6.5k lines;
  - native C: ~0.5k lines (Landlock launcher and flock).

  The core loop is small. `packages/core/agent-loop/src` is about 2.5k lines, `core/session/src` about 3.3k, and `core/tools/src` about 5.7k.
- **Maturity:** version `0.2.1-alpha.2` (`package.json`). `SAFETY.md` calls it an "experimental developer-preview… has not undergone a security audit". Public APIs are declared pre-stable (`AGENTS.md`). The visible git history starts 2026-07-30 at PR #893 and has about 6.2k commits; the latest commit is 2026-10-09. The project has heavy process machinery: 764 implemented Agent Notes (ADR-style records) in `.agents/notes/implemented/` and 74 `scripts/verify-*` gates.

## 2. Repository layout

- `vendor/`: the pinned Cordis framework family.
- `packages/core/`: the product spine.
  - `session`: the event log and surface.
  - `agent`: the `Agent` interface, registry and `agent/*` events.
  - `agent-loop`: the default driver (`ReactLoopAgent`).
  - `tools`: the tool registry, execution pipeline and PTC mode.
  - `system-prompt`: assembly of prompt sections and tool schemas.
  - `scope`: per-agent registration scopes.
- `packages/llm/`:
  - `llm`: the abstract `LlmRuntime` and `LlmAdapter` seam.
  - `llm-deepseek`: a DeepSeek Messages transport over Anthropic-compatible `/anthropic`.
  - `llm-deepseek-api-key` and `llm-deepseek-account`: authentication plugins.
  - `llm-pi-ai`: multi-provider routing through `@earendil-works/pi-ai`.
  - `llm-retry`, `token-meter`, and `deepseek-llm-api-extensions` (DeepSeek-only wire extensions).
- `packages/session/`:
  - JSONL persistence;
  - adjacent format migrations `v0-to-v1` through `v3-to-v4`;
  - projections and the projection cache;
  - checkpoint policy;
  - titles and telemetry;
  - `session-log-deepseek` (session-log upload);
  - the working-directory capability.
- `packages/session-query/`: logical corpus reads, lineage tracing and SQLite FTS5 search.
- Execution: `shell/`, `subprocess/`, `terminal/` (PTY), `ssh/` (remote filesystem, subprocess and sandbox providers), `sandbox/`, `fs/`, `lsp/`, `ptc-runtime/`.
- Agent features: `compaction/`, `context/`, `skill/`, `subagent/`, `jobs/`, `workflow/`, `schedule/`, `goal/`, `plan/`, `todo/`, `guard/`, `deliverables/`, `spill/`, `attachment/`, `web/`, `mcp/`, `computer-use/`, `browser-use/`.
- Composition: `preset/` (per-session agent composition) and `bundle/` (the profile patch layers `base`, `web-app`, `headless`, `sdk-app`, `sdk-minimal` and `acp-app`).
- Surfaces:
  - `sdk/` (JSON-RPC protocol, client and server);
  - `acp/` (Agent Client Protocol server);
  - `api/` (the Remote back-end-for-frontend layer and the Typert gateway);
  - `typert/` (build-time type-graph RPC codegen);
  - `host/` (web server, static frontend, directory picker);
  - `client/` (63 browser plugins, React UI);
  - `boot/` (app-boot, cmdline, hmr, plugin-manager);
  - `extensions/` (runtime Cordis inspection and dynamic plugins).
- Cross-cutting: `settings/`, `credentials/`, `storage/`, `interaction/` (approval, questions, commands, permission presets), `identity/`, `telemetry/`, `util/`, `test-support/`.
- `experimental/`: hooks bridges for Claude Code and Codex hooks, a Claude Code mods bridge, agent teams, Ralph, auto-review, a Python PTC runtime, browser and computer use, session search, webhooks, worktree and voice.
- `apps/`: `cli` (the `dsh` launcher), `web` (the Vite SPA), `desktop` and `desktop-host` (Electron).
- `native/`: the Linux Landlock launcher and POSIX flock.
- `python/`: the SDK and runtime wheel.
- `snapshots/`: recorded sessions used for replay tests.
- `scripts/`: gates and generators.
- `docs/`: bilingual English/Chinese documentation.
- `website/`: a VitePress projection of the docs.

## 3. Process topology

- **One launcher.** `dsh` is "the sole supported Node application launcher" (`apps/cli/README.md`). `dsh <profile>` or `dsh --profile <name>` boots a profile. The shipped profile templates are `web`, `headless`, `sdk`, `sdk-minimal` and `acp`. `desktop` is reserved for Electron, and `dsh plugin …` forwards to pnpm inside a profile directory. Package bins and inline Cordis trees are forbidden as launch paths, and `scripts/verify-application-entrypoints.ts` enforces that (`docs/architecture.md:41-47`).
- **Profiles and bundles.** A profile lives at `$DSH_HOME/profiles/<name>`; the default home is `~/.dsh` (`packages/util/home-paths/src/index.ts:12-18`). It contains:
  - a `package.json` whose `dsh.profile.bundles` lists ordered bundles;
  - its own `node_modules` for out-of-tree plugins, installed with pnpm;
  - a user `cordis.patch.yml`.

  A bundle is an npm package with `dsh.bundle.patch` pointing at a patch YAML (`packages/bundle/base/package.json`). The boot sequence composes over an empty root in this order:
  1. each bundle patch;
  2. the profile patch;
  3. `$DSH_HOME/cordis.patch.yml`;
  4. `--patch` overlays (`docs/architecture.md:25`).

  `dsh --profile web --dump-config` prints the effective tree.
- **Processes:**
  - **Headless:** one process; it runs one task, prints the answer and exits, with `--json` event streaming and `--session-id` resume (`packages/bundle/headless/README.md`). There is no interactive TUI in the repository. The README mentions a `tui` profile only as a hypothetical example (`apps/cli/README.md`).
  - **Web:** a Node Host process. `dsh-host-webserver` is a `node:http` or `https` server with named routes, upgrade routes and a fallback. It serves the SPA and the `/api` gateway. The browser runs its own Cordis plugin tree (the client half). Typed RPC uses Typert-generated descriptors over HTTP and WebSocket streams (`packages/api/gateway/src/stream-server.ts`). Live session events reach the browser through a "Session-follow adapter" (`docs/architecture.md:106`).
  - **Desktop:** Electron main, plus an Electron-RunAsNode child running the shared profile runner (Desktop Host), plus a renderer that loads packaged web assets. Node IPC carries boot injections, readiness and shutdown, while HTTP and WebSocket traffic goes to the authenticated loopback Host on an OS-assigned port (`docs/architecture.md:49-53`; `apps/desktop/README.md`).
  - **SDK:** `dsh --profile sdk` serves newline-delimited JSON-RPC 2.0 on stdio. Methods are `initialize`, `session/prompt`, `session/wait`, `session/working-directory/get` and `session/working-directory/set`, and `shutdown`. Notifications are `session.event`, `session.status`, `subagent.started` and `subagent.finished` (`packages/sdk/protocol/src/types.ts:131-146`). The TypeScript `DeepSeekHarness` client and the Python SDK both spawn `dsh` as a subprocess.
  - **ACP:** `dsh --profile acp` is an automation-only Agent Client Protocol server. It supports create, resume, model and effort selection, attaching MCP servers, cancel and close; it does not support fork or transcript replay (`packages/acp/acp/README.md`).
  - **Subagent subprocesses (optional):** ACP children, fresh `dsh` SDK children, and pinned Claude Code or Codex runtimes (`packages/subagent/*`).

## 4. Plugin architecture

- **Cordis basics** (`docs/cordis-primer.md`):
  - A plugin is a function or object with `inject` and `apply(ctx, config)`, or a `Service` subclass.
  - A context is a registry of services addressed as `ctx.<key>` (`ctx.tools`, `ctx.llm`, `ctx.sessions`, …).
  - `inject` declares dependencies, and activation waits until those services exist. Load order is therefore driven by service availability, not by list order (`packages/bundle/base/cordis.patch.yml:12-13`).
  - Events are typed through TypeScript declaration merging. Each one has a declared dispatch mode, documented with an `@mode` tag and checked by a generator: `emit`, `waterfall` (around-middleware, where a listener must call `next()` to delegate), `parallel`, `serial` or `bail`.
  - Every registration is a reversible effect (`ctx.effect()` / `ctx.on()` returning a disposer). Unloading a plugin therefore unwinds its contributions, which is what makes HMR and runtime enable/disable safe.
- **Composition.** `cordis.yml` rows are `{id, name, config, disabled, group, isolate}`. A patch can `insert` rows or replace a row's whole `config` by id. `!!js` expressions are allowed only in `config` and `disabled`, for example `disabled: !!js process.platform === 'win32'` and `mode: !!js process.env.DSH_PERMISSION_MODE ?? 'workspace-write'` (`packages/bundle/base/cordis.patch.yml:239-262`). `cordis:group` with `isolate:` gives a sub-tree its own instance of a service name. HMR (`dsh-hmr`) watches the profile YAML and recomposes in place.
- **Capability seams.** Every capability has three roles (`docs/glossary.md`):
  - a **Service Definition**: an abstract `Service` class owning `ctx.<key>`;
  - one or more **Service Providers**;
  - one or more **Consumers**, often a model-facing tool.

  The canonical example is `dsh-shell` (definition), `dsh-bash-local` and `dsh-bash-sandbox` (providers), and `dsh-tool-bash` (consumer). Extension plugins must depend on definitions, never on concrete providers (`packages/README.md`). Because filesystem and subprocess providers share one "execution world", swapping them for SSH providers moves bash, PTY and LSP together (`docs/architecture.md:134`).
- **Presets.** `@deepseek-ai/dsh-agent-preset` declares a named list of child plugin rows. The registry mounts it under the agent's scoped `agent.ctx` during the agent's setup window (`packages/preset/agent-preset-registry/src/index.ts:277-353`). Web ships the `standard`, `minimal`, `ptc` and `cordis` (creator mode) presets (`packages/bundle/web-app/presets/*.patch.yml`). Inside a preset, persona, tools, plan mode and compaction can each be grouped with `isolate`. The chosen preset id is persisted in the `SessionHeader` as `agentPreset` (`packages/core/session/src/types.ts`), so a resume restores the same composition.
- **Scopes.** A registration is either global or scoped to exactly one agent. Scoped tools, sections and variables shadow same-named globals. `tools.restrict` intersects the global set for one scope. A filtered-away tool is absent from the prompt and refuses execution (`docs/glossary.md`).
- **Concrete plugin example.** `packages/todo/tool-todo/src/index.ts` exports `name = 'tool-todo'`, `inject = ['tools','sessionProjections']`, and a Schemastery `Config` with a required `allowParallelInProgress`. `apply(ctx, config)` does two things:
  1. It registers a `todos` session projection, a pure fold in which `todo/write` replaces the list and `turn/start` clears it (lines 123-134).
  2. It registers `todo_write` through `defineTool({name, description, parameters, output:{schema, render}, execute})` (lines 135ff).

  Each call appends a durable `todo/write` session event, and the UI renders from that event.

## 5. Agent loop

The loop lives in `packages/core/agent-loop/src/agent.ts` (`ReactLoopAgent`, about 700 lines).

- **Inbox.** `followup()` targets the next turn and wakes the loop. `steer()` targets the next step and wakes it. `inject()` targets the next step without waking it (`agent.ts:163-173`). Injected context therefore waits for a waking message.
- **Turn** (`agent.ts:298-386`):
  1. Append `turn/start`.
  2. Loop over steps. `preStep` claims inbox input, assembles prompt sections and tool schemas, then runs the `agent/pre-step` waterfall (`agent.ts:267-290`). That waterfall can reject the step (`turn/end` with reason `blocked`), rewrite the messages, or declare `startsRequestSeries`.
  3. Append `step/start` and run the step.
  4. When nothing is owed, run `agent/turn-stopping`, a serial event (`agent.ts:362`). A listener can call `agent.steer()` to force another step; the goal driver and the hooks bridges use this.
  5. Append `turn/end` with reason `completed`, `max-tokens`, `blocked`, `aborted` (with cause `user`, `parent`, `hook` or `disposed`) or `error` (with a structured `LlmFailure`).
- **Step** (`agent.ts:400-560`):
  1. `prepareRequest` runs the `agent/request` waterfall over the call configuration (provider, model, reasoning effort, maxTokens), then `ctx.llm.prepareCall()` (`agent.ts:564-612`).
  2. The system prompt is projected as `system/message` events. On routes that support in-history updates, a changed prompt is appended after cached history instead of rewriting the prefix.
  3. Accepted user messages and runtime context are appended as `user/message`.
  4. `buildRequest` logs `request/header` (reasons `initial`, `resume`, `change` or `series`) and `request/context`. Tool additions and removals are logged as a `developer/message`. Model history is then derived from the log with `session.deriveMessages()` and deep-frozen (`agent.ts:616-704`).
  5. The request streams through `preparedCall.stream()`. Chunks go out as the process-local `agent/assistant-stream` event (start, chunk and end frames). The finished stream is committed once as `assistant/message`, which embeds the compact timed stream, or as a log-only `assistant/attempt` for failed, cancelled or retried attempts.
  6. On a stream failure, the `agent/request-error` waterfall may return `{kind:'retry'}`; `llm-retry` uses this. Compaction and image-offload also hook this event to compact or offload and then retry.
- **Tool dispatch** (`packages/core/agent-loop/src/tool-calls.ts`):
  - Each call's `ToolDefinition.isConcurrencySafe(args)` classifies it as parallel or exclusive (`packages/core/tools/src/index.ts:281`).
  - Exclusive calls form barriers. Parallel calls use a rolling pool capped by `maxParallelToolCalls` (default 10, a volatile setting; `constants.ts:6`, `index.ts:346`).
  - Pre-execute policy runs in model order; only dispatch overlaps. Results are committed to the log strictly in model order, and each `tool/result` cites its `tool/call` seq.
  - On abort, started calls drain and unstarted calls receive synthetic `ABORTED_BEFORE_DISPATCH` error results, so the log stays replayable. On a step failure, `ToolCallRecovery` writes conservative results for any owed calls.
- **Tool pipeline** (`docs/tool-execution-pipeline.md`): `tool/call` is logged first. Then:
  1. `tools/pre-execute` waterfall, deciding `allow`, `deny`, `cancel` or `ask` (`index.ts:608`);
  2. registered monotonic guards;
  3. `ctx.approval` for `ask`;
  4. `tools/execute` around-waterfall (timeouts, retries);
  5. the tool body;
  6. `projectContent`;
  7. `tools/post-execute`, deciding `accept`, `block` or `replace`, and able to add context;
  8. `finalizeContent`;
  9. a synchronous `tools/result` notification;
  10. the `tool/result` log entry.

  A tool result can set `concludesTurn` to end the turn early.
- **Stop conditions.** The loop has no step-count cap; grep finds no `maxSteps` or `maxTurns` in the packages. A turn ends when:
  - the assistant makes no tool calls;
  - the response hits `max-tokens`, which is sticky for the rest of the turn;
  - a tool result concludes the turn;
  - a pre-step reject, abort or error occurs.

  Runaway prevention comes from plugins:
  - `dsh-repeat-tool-reminder` (advisory reminders at 3, 5 and 8 identical repeats; `packages/bundle/base/cordis.patch.yml:444`);
  - `dsh-tool-call-timeout-policy` (cooperative per-tool `timeoutMs` through `tools/execute`; `packages/guard/timeout-policy/src/index.ts:25`);
  - the goal round cap (default 256).

## 6. Model/provider layer

- **Seam.** `LlmRuntime` (`ctx.llm`) is an adapter registry. `LlmAdapter` is abstract, with `prepareCall()` and `stream()` (`packages/llm/llm/src/index.ts:208-290`). The `llm/stream` waterfall lets middleware wrap streams (`index.ts:75`). Every stream ends with a terminal result chunk, and adapters normalize failures into `LlmFailure`.
- **Prepared calls.** A `PreparedLlmCall` reports capabilities that drive the loop: `systemPromptUpdate: 'in-history'`, `toolUpdate`, context window, `retryPolicy` and `adapterDefaults`.
- **DeepSeek adapter.**
  - It speaks the Anthropic Messages wire to `https://api.deepseek.com/anthropic`, sending `anthropic-version: 2023-06-01` and optional betas (`packages/llm/llm-deepseek/src/config.ts:106`, `adapter.ts:130`). It includes a Files API upload index for images and attachments.
  - Two authentication plugins exist: `deepseek-official` uses an API key in `x-api-key`; `deepseek-account` uses an account token.
  - DeepSeek-specific body extensions are sent by default: `dsh_plugin_packages` (the active plugin inventory) and `dsh_session_log`. The second uploads the canonical session-log suffix incrementally, with an acceptance watermark recorded as a durable `session-log-deepseek/delivery-accepted` event. Headers include an anonymous user id and the session id (`docs/deepseek-llm-api-wire-extensions.md`). Both extensions can be disabled.
- **Other providers.** `dsh-llm-pi-ai` routes to any pi-ai provider, OpenAI-compatible gateway or self-hosted server, with OAuth or interactive sign-in and cross-process refresh locking (`packages/llm/llm-pi-ai/README.md`).
- **Retry.** `llm-retry` retries at durable step boundaries, logging `llm/retry` events before backoff. It has bounded and unbounded "always" modes.
- **Token meter.** `token-meter` estimates context pressure deterministically from the log, with no model calls. Compaction and UIs share its results.

## 7. Tool system

- **Definition.** `ToolDefinition` (`packages/core/tools/src/index.ts:224-300`) has:
  - a JSON-schema `parameters` object;
  - a **mandatory output schema** with a pure `render()` from value to model content blocks and an optional `presentationMeta`;
  - `execute(args, exec)`, which returns a lossless-JSON value and must honor `exec.signal`;
  - optional `projectContent` and `finalizeContent`;
  - `timeoutMs` (never sent to the model);
  - `isConcurrencySafe`;
  - pure UI presenters `presentCall` and `presentResult`, safe to replay from the log.

  Tools register with `ctx.tools.register(defineTool(...))`, and the registry emits `tools/change`.
- **Built-in tools.** The generated `docs/tool-catalog.md` lists them. Most are in base or presets; some are opt-in.
  - Shell: `bash` (or `pwsh` on Windows), with an optional `run_in_background` and a `sandbox_permissions` escalation parameter. A persistent PTY variant of `bash` also exists.
  - Files: `read`, `write`, `edit`, `read_image`.
  - Search: `glob` and `grep`, using bundled ripgrep through `ctx.subprocess`.
  - `str_replace_editor` (opt-in).
  - Jobs: `job_list`, `job_output`, `job_kill`.
  - `todo_write`.
  - Goals: `create_goal`, `get_goal`, `update_goal`.
  - `exit_plan_mode`.
  - `skill`.
  - Subagents: `subagent`, `subagent_fork`, `list_subagent_models`, `send_message`, `interrupt_agent`, `list_agents`.
  - `workflow`.
  - Web: `web_search` and `web_fetch`.
  - `working_directory`.
  - MCP resources: `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource`.
  - `ask_user_question`.
  - `present` (deliverables).
  - Schedules: `schedule_create`, `schedule_list`, `schedule_update`, `schedule_delete`.
  - `lsp`.
  - Terminals: `terminal_open`, `terminal_send`, `terminal_read`, `terminal_list`, `terminal_signal`, `terminal_close`.
  - `plugin_manager`.
  - Cordis inspection: `cordis_inspect_list` and `cordis_inspect_query`.
  - `run_code` (PTC mode).
  - Experimental: `ralph`, the agent-team tools, `create_worktree`, the session-query tools, and Stagehand browser tools.
- **Edit strategy.** `edit` does literal string replacement with `file_path`, `old_string`, `new_string` and `replace_all`. The old string must be unique unless `replace_all` is set (`packages/fs/tool-fs/src/edit.ts:33-96`). `dsh-fs-observation-policy` adds read-before-write and stale-file checks as listeners on `fs/write-intent` and `fs/edit-intent`. Observations are not persisted, so a resumed session must re-read files. Diff cards come from `presentResult` metadata.
- **Output spill.**
  - `spill-policy` keeps tool results under a shared token budget (`maxInlineTokens: 12500` in base). The model gets head and tail content plus a file path to the full result (`packages/bundle/base/cordis.patch.yml:410`).
  - `compaction-tool-result-pruner` separately head/tail-trims oversized results (threshold 8192 characters) once compaction pressure qualifies. The originals remain in the log.
- **PTC (programmatic tool calling).** Setting `tools` config `mode: 'ptc'` (`packages/core/tools/src/index.ts:686`) changes what the model sees:
  - The model receives only one tool, `run_code`, plus a system-prompt section containing a generated TypeScript SDK, or a Python SDK through the experimental runtime (`jsonSchemaToTs`, `renderToolsSdk`, `renderToolsSdkPy` exports).
  - The model writes an async TypeScript program. Its tool calls are bindings that re-enter the full guarded pipeline (policy, approval, logging) with a `parent` token. A model-direct call to a native name is denied.
  - `ptc-runtime-node` runs each program in a fresh managed Node process under the session's sandbox policy. Limits: a 120 s default and 600 s maximum timeout, a 512 MB heap, and at most 128 pending calls (`packages/ptc-runtime/ptc-runtime-node/README.md`).
  - Sub-calls are logged as `tool/ptc-dispatch-start` and `tool/ptc-dispatch`. These events are log-only and never re-enter model context; only the program's return value and logs do (`packages/core/tools/src/types.ts`).
  - The same process engine backs the `workflow` tool.

## 8. Context engineering

- **Assembly.** `dsh-system-prompt` (`ctx.systemPrompt`) orders the sections:
  - the fixed identity "You are an AI agent powered by DeepSeek Harness." (`HARNESS_IDENTITY: -1000` at `packages/core/system-prompt/src/index.ts:132`);
  - the persona prefix;
  - plugin sections, each rendered only while its tool is visible to the agent;
  - environment suffixes.

  It supports variables, scoped shadowing, a validated `toolOrder`, and a `system-prompt/assemble` waterfall. Dynamic runtime context (sandbox mode, time, working directory) is projected as sourced `user/message` snapshots, not as system-prompt text.
- **"Model-visible ⟺ logged".** Every model request must be reconstructable from the session log (`docs/architecture.md:124`). The loop derives each request's messages from the log immediately before sending (`agent.ts:688`), and loop-built requests are deep-frozen so adapters cannot alter them (`packages/llm/llm/README.md`). Plugins that change existing message content must register pure "message projections" over durable events.
- **Surface.** The log is append-only. The "surface" is an ordered view of the message-producing events (`system/`, `developer/`, `user/` and `assistant/message`, plus `tool/result`). A message event carries `surfaceOp: 'append'` or `{op:'replace', startSeq, endSeq}` (`packages/core/session/src/types.ts:448-465`).
- **Compaction.**
  - `compaction-basic` listens on `agent/pre-step`, and triggers at a 0.8 context-pressure ratio with 0.16 retention (`packages/compaction/compaction-basic/src/config.ts:20-23`). It also reacts to `agent/request-error` on overflow. It makes one summarization model call.
  - It appends `compaction/summary` (log-only) and a surface `replace` event covering the condensed range (`region.ts:491-507`). The shadowed messages stay in the log, so replay reproduces the condensed view deterministically.
  - `/compact` is the human command for it. `compaction-image-offload` replaces old images with path placeholders when a route rejects the image budget.
- **KV-cache awareness.** Every package README carries a required "Model Experience" section with a "KV Cache effect" subsection, enforced by `scripts/verify-package-readme-model-experience.ts`. Design choices that protect the cached prefix:
  - in-history system-prompt updates;
  - tool deltas logged as developer messages;
  - plan mode keeping the tool catalog unchanged "for request-cache stability" (`packages/bundle/web-app/presets/standard.patch.yml`);
  - fork subagents pinned to the parent's model "for KV Cache reuse" (`packages/bundle/base/cordis.patch.yml:376-390`).
- **Instructions and skills.**
  - `agent-instructions` loads user-global and project `AGENTS.md`/`CLAUDE.md` chains (`packages/context/agent-instructions/src/config.ts:12`), discovers nested files after filesystem operations, and is byte-budgeted.
  - Skills come from `.dsh/skills`, `.agents/skills`, `~/.dsh/skills` and the agents-home `skills` directory (`packages/skill/skill-filesystem/src/index.ts:249-257`).
  - Before the first request, the model gets a durable catalog of names and capped descriptions. The `skill` tool loads a skill's full text, and `/name` lets users invoke one.
- **Goals, plan and todo.**
  - Goals are durable, revisioned same-session objectives with phases `active`, `paused`, `blocked` and `complete`. `goal-round-driver` keeps continuing while the agent is idle and continuation is "armed"; arming is deliberately process-local, so a resume needs human re-authorization.
  - Plan mode is a prompt-policy mode exited through the `exit_plan_mode` approval. It does not restrict tools; enforcement is left to the sandbox and approval.
  - `todo_write` is a whole-list replacement stored as session events.

## 9. Sessions and state

- **Event-sourced log.** `SessionEvent` is a discriminated union over an extensible `SessionEventMap` (`packages/core/session/src/types.ts:495-515`). Each event carries `type`, monotonic `seq`, `time`, `data` and optional `ignorable: true`. Unknown events without `ignorable` force the reader to refuse reconstruction. `known-event-types.ts` lists about 60 types, including `turn/*`, `step/*`, `request/header`, `assistant/attempt`, `tool/call`, `approval/asked`, `compaction/*`, `goal/change`, `plan/mode`, `sandbox/mode` and `subagent/descriptor`. Experimental plugins write `plugin:`-namespaced records outside the released schema.
- **Versioning.**
  - `SESSION_FORMAT_VERSION = 4` (`types.ts:89`); v4 is released, with evidence tag `dsh-v0.2.0-rc.2` (`docs/session-format-status.md`).
  - Each migration package converts exactly one `vN→vN+1` step. Opening an older session for writing publishes a new version-named file, `session.vN.jsonl[.zstd]`, beside the untouched original. Committed generations are never renamed or deleted, and there is no downgrade (`docs/architecture.md:122`).
  - Persistence-type changes require explicit acknowledgement records (`docs/persistence-changes/`).
- **Storage.**
  - Sessions are stored as JSONL per session, zstd-compressed with checksummed frames by default, with torn-tail crash recovery (`packages/session/session-persistence-jsonl`). Write locking uses native POSIX flock.
  - `session-checkpoint-policy` fails closed: work is made durable before each model request and before each top-level tool body, and a tool interrupted by a crash recovers with an "unknown outcome" instead of being retried.
  - SQLite (`node:sqlite`) is used only for derived and secondary data: the `session-query-sqlite` FTS5 index (schema version 8) and the `storage-sqlite` backend for non-session domains. JSON-file storage is the default domain backend in base.
- **Replay and reconstruction.**
  - `deriveMessages()` (`packages/core/session/src/index.ts:984`) projects model history from the log.
  - Fork is `ctx.agents.create({seed, meta:{parentSession, seedLength}})`, with a `session/end-seed` marker at the fork point.
  - `session-projection` registers incremental folds (todos, turn boundaries, goals) that clients read as typed snapshots, and `session-projection-cache` checkpoints them.
- **Query.** `session-query` provides bounded reads, lineage traces and semantic filtering. `session-log-export` exports logs. The opt-in experimental `tool-session-query` gives the model five read-only search and trace tools over past sessions.

## 10. Permissions, safety, sandboxing, credentials

- **Modes.** There are three file-effect modes: `read-only`, `workspace-write` (the default) and `danger-full-access`. They are set per session through `sandbox-policy` and survive restart.
  - The approval policy is `ask`, or `never` under full access (`packages/bundle/base/cordis.patch.yml:239-272`).
  - `permission-presets` bundles a sandbox mode with an approval policy.
  - The model is told the effective policy before each request.
- **Approval model: sandbox first, approval on escalation.** Under the shipped defaults, bash and file tools run without per-command approval but inside the sandbox. The model asks for more access by passing `sandbox_permissions` (one strictly wider mode, per `WIDER_MODES` in `packages/sandbox/sandbox/src/escalation.ts:27`) together with a justification. That request goes to `ctx.approval`, and approval is one-shot and fails closed when no answerer is available. Generic `ask` decisions also come from hooks bridges and experimental Auto-review, an LLM judge.
- **Confinement backends** (`packages/sandbox/sandbox-local/src/index.ts:1-20`):
  - Linux: bubblewrap, falling back to a Landlock launcher (a static C binary in `@deepseek-ai/node-addon-system`).
  - macOS: Seatbelt (`sandbox-exec`).
  - Windows: a restricted-token, Low-integrity ACL runner (`sandbox-windows-acl`).

  Backends are probed functionally. An unavailable backend yields `SANDBOX_UNAVAILABLE` rather than an unconfined run, and results report `full` or `partial` enforcement plus denial signatures. Network access and process visibility are explicitly out of scope (`packages/shell/bash-sandbox/README.md`). `fs-sandbox` separately confines the harness's own file-tool writes, and PTC and workflow processes run under the same policy.
- **Credentials.** `credentials-local` resolves secrets in this order: launch environment, then a private file in the harness home, then the project `.env`, then the home `.env`. Configuration refers to key names, never values. The README notes that the store cannot isolate secrets from agent processes running as the same user. `authorization` covers human-guided sign-in flows. `SAFETY.md` disclaims any security guarantee.

## 11. Extensibility

- **Plugins:** anything can be added as a Cordis plugin in a bundle or profile patch. Plugin Manager and `dsh plugin` run pnpm installs into the profile, enforcing peer-version ranges and requiring explicit exemptions for mismatches.
- **Runtime self-modification (`extensions/`).**
  - In "creator mode" (the `cordis` preset), the agent has read-only `cordis_inspect_*` tools and installs persistent bundles through `plugin_manager`.
  - `cordis-host-runner` can run process-local dynamic definitions in a `node:vm` realm, paired with a browser-side runner, for programmatic callers. No model tool creates these dynamic definitions.
  - The `agent-preset` package ships skills for writing Cordis plugins.
- **MCP:** `mcp-client` exposes tools named `mcp__server__tool`; it is off by default. Server instructions join the logged system prompt. Base composes `mcp-resources`. ACP clients can attach MCP servers per session.
- **Subagents:** one `ctx.subagents` seam with these providers:
  - `spawn`: in-process, fresh history;
  - `fork`: in-process, seeded with the parent's completed turns;
  - `acp`: an ACP subprocess;
  - `dsh-sdk`: a fresh `dsh` subprocess;
  - `claude-code` and `codex`: genuine pinned runtimes, one-shot.

  Local children are continuable through `send_message`, and delegation returns a child id promptly. `delegationDepth` is persisted for recursion budgets.
- **Jobs:** `ctx.jobs` gives background bash, PTY sends and workflows stable `<kind>-N` ids, scoped to the owning agent. Completion arrives as an injected notice.
- **Workflow:** a JavaScript orchestration script using `agent()`, `parallel()`, `pipeline()`, `phase()` and `log()`, run in a fresh sandboxed Node process. The experimental Ralph loop (fresh child per round, with a bounded handoff) builds on it.
- **Schedule:** one-shot, fixed-rate, daily, weekly and cron follow-ups that resume the original session, including cold sessions, after Host restart.
- **Hooks:** there is no native hooks configuration. Experimental bridges run existing Claude Code or Codex `hooks.json` command hooks on top of the loop events (`packages/experimental/hooks-*`). An experimental `claude-code-mods` bridge runs Claude Code mods.
- **Agent teams (experimental):** a lead, teammates, direct inboxes and a durable task board.

## 12. Clients and UI

- **Web and Desktop are the interactive UIs.** The client is a second Cordis plugin tree running in the browser, with 63 `client/` packages.
  - `ui-slots` provides typed extension regions.
  - `ui-renderer` mounts React 18 after all client plugins activate.
  - `client/store` is built on Zustand and Immer, and list rendering uses `@tanstack/react-virtual`.
  - Plugins cover chat, the trajectory view, tools, approvals, plan, goal, jobs, schedule, subagents, the file, terminal and browser sidebars, and settings pages.
- **Rendering from durable data.** Chat nodes are assembled from raw session events through `ConversationNodeDefinition` registries. Tool cards come from pure presenters plus persisted `meta`, so live and replayed views match.
- **Localization.** All UI copy is locale-owned, with English and Chinese typed dictionaries enforced by `verify-client-ui-i18n`.
- **No TUI.** The CLI's only non-GUI modes are headless (one-shot), SDK and ACP.
- **Desktop** wraps the same Web app, adds a "Manage dsh Command" installer, and bundles pnpm.

## 13. Configuration and settings

- **Configuration is the composed `cordis.yml` tree.** It is built from bundle patches, then the profile patch, then the home patch, then `--patch`. Every plugin `Config` is a validated Schemastery schema, and invalid config fails at load. A repository rule forbids hard-coded tunables: "No hardcoded tunables in plugins" (`AGENTS.md`).
- **Introspection.** The `docs/config-catalog.md` reference is generated. `--dump-config` prints the effective tree and `--dump-config-schema` prints its JSON Schema.
- **User settings.** `dsh-settings` edits plugin fields declared `.volatile()` and writes them back into the profile's Cordis patch with stale-write refusal. Settings and configuration are therefore one store. Volatile fields such as `maxParallelToolCalls` apply without restart, and HMR recomposes on YAML edits.
- **Environment knobs.** Examples include `DSH_HOME`, `DSH_PERMISSION_MODE`, `DEEPSEEK_API_KEY` and `DEEPSEEK_BASE_URL`.

## 14. Testing and quality infrastructure

- **Unit:** vitest, with a **per-file 100% coverage gate** on `packages/*/*/src` (`pnpm run test:coverage`). Every registry has an HMR-safety test (`docs/testing.md`). Tests prefer real implementations and mock only the LLM adapter, the network or the clock.
- **Snapshot replay:** about 571 recorded session JSONL fixtures under `snapshots/{session,sdk,acp,web}`. They replay keylessly through shipped profiles. Each recorded session supplies user input and model replay, and is also the expected persisted output. Every non-trivial model-visible change must update one, and the TypeScript and Python SDK expected outputs must be updated too. Web snapshots run in Chromium, and the model picker also runs in WebKit.
- **Other tiers:**
  - Real-API e2e. The policy reads: "We are DeepSeek — do not ration real-API tests."
  - Built-artifact smokes under plain Node.
  - Performance benchmarks as a required gate.
  - A Windows/Wine lane.
- **Static and documentation gates:** about 74 `verify-*` scripts. They cover:
  - JSDoc on every export;
  - no new `as unknown` casts;
  - type-equivalence of pasted docs;
  - doc word budgets;
  - one-line paragraphs;
  - bilingual pairing;
  - README "Model Experience" and "Known Limitations" sections;
  - `cordis.yml` dependency closure;
  - application entrypoints;
  - product-use classification;
  - a ban on metaphorical terms (`verify-concrete-terms`);
  - cross-file clone detection (`pnpm run duplication`).

## 15. What is distinctive

1. **Everything is a swappable plugin, including the loop.** The agent loop, tool registry, session log, prompt assembler and LLM runtime are all Cordis services addressed by key and replaceable from YAML. There is no privileged core.
2. **Profile and bundle layering as the product model.** Web, headless, SDK and ACP are the same `dsh` binary with different ordered YAML patch stacks. Users patch any row by id, and out-of-tree plugins install with pnpm into a profile.
3. **A strict event-sourced invariant ("model-visible ⟺ logged").** Each request is derived from the log at send time and frozen. Compaction is a surface `replace` that keeps the originals, and assistant streams are embedded in the log. Replay is exact.
4. **Released-format discipline for session data.** An integer format version, adjacent single-step migration packages, immutable version-named file generations, `ignorable` markers, and acknowledgement records for persistence-type changes. This is unusual rigor for an alpha.
5. **Explicit KV-cache engineering as a documented design axis.** Per-package "KV Cache effect" docs, in-history system-prompt updates, a stable tool catalog across modes, and model-pinned forks.
6. **First-class PTC mode.** A single `run_code` tool plus a generated TypeScript or Python SDK. Programs run in sandboxed Node processes, and every nested call re-enters the full policy pipeline and is logged.
7. **Sandbox-first permissions.** OS confinement with bwrap, Landlock (via its own native launcher), Seatbelt or Windows ACLs fails closed. Approval is reserved for model-requested escalation to a wider mode, not for each command.
8. **Capability seams with execution-world coherence.** Swapping the fs, subprocess and sandbox providers (for example to SSH) moves every tool together.
9. **Inbox semantics.** `followup`, `steer` and `inject` give three admission classes, with step-boundary steering and a serial `turn-stopping` hook that can veto ending the turn.
10. **Heterogeneous subagent providers behind one seam.** In-process spawn and fork, ACP, `dsh`-SDK subprocesses, and genuine Claude Code or Codex runtimes. Hooks bridges also run Claude Code and Codex hook configurations.
11. **The browser UI is itself a Cordis plugin tree.** Typed Host↔Client RPC is generated by Typert.
12. **DeepSeek-specific telemetry defaults.** The official adapter uploads the session-log suffix and the plugin inventory as body fields by default. They can be disabled.

## 16. Strengths, weaknesses, lessons

**Strengths**
- Extreme replaceability and composability. Policies such as timeouts, retries, spill, compaction, approval and hooks are small plugins on documented waterfalls rather than loop edits.
- Durability and correctness are taken seriously:
  - fail-closed checkpoints before side effects;
  - synthetic results for aborted calls;
  - ordered result commits under parallel dispatch;
  - crash-tail repair;
  - exact replay.
- The log is a single source of truth that serves UI replay, fork, resume, telemetry, search and tests alike.
- Strong test strategy: recorded-session replay through real shipped profiles, a 100% per-file coverage gate, and real-API e2e.
- Multiple first-class embedding surfaces (SDK in TypeScript and Python, ACP, Web, Desktop) share one runtime.

**Weaknesses and risks**
- Very high conceptual and process overhead: 331 packages, about 900k lines including tests, 764 decision notes, 74 gates, bilingual documentation with word budgets. Onboarding and velocity costs are likely high (inferred).
- Cordis is a niche framework. Waterfall semantics, where forgetting `next()` silently short-circuits, and service-availability load ordering are a source of subtle bugs; the repository has rules and gates specifically for this.
- No terminal UI. Interactive use requires Web or Desktop, unlike opencode or grok-build.
- No hard step or turn cap in the loop. Loop control is advisory (repeat reminders) or plugin-provided, and the timeout policy is cooperative only.
- Plan mode is prompt-only, with no tool restriction. Network access is not sandboxed. The credential store cannot hide secrets from tools running as the same user.
- DeepSeek-centric defaults: the session-log upload is on by default, and the official adapter is Anthropic-wire-based. Multi-provider support goes through pi-ai.
- Still alpha, with explicitly pre-stable APIs.

**Lessons for building a harness**
- Make the session log the only source of model input, and derive each request from it. That makes fork, resume, compaction, UI replay and testing almost free.
- Put interception on a small set of typed waterfalls around pre-step, request, request-error, pre-execute, execute and post-execute, plus a turn-stopping veto. Keep policies as plugins.
- Commit tool results in model order even when dispatch overlaps, and always write a result for every call, synthetic if needed.
- Treat the KV-cache prefix as a design constraint. Append prompt and tool deltas instead of rewriting them, and keep tool catalogs stable across modes.
- Prefer fail-closed OS sandboxing with explicit escalation over per-command approval prompts.
- Version the persisted format from day one, and test by replaying recorded real sessions through the shipped composition.
- Be cautious about copying the process overhead: much of the rigor here serves a large organization, not a small team (inferred).
