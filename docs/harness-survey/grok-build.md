# Grok Build (`grok`): architecture report

Snapshot: [DangerousMindsAi/grok-build](https://github.com/DangerousMindsAi/grok-build) at `2bdd1d6` (2026-09-29), a read-only mirror of the xAI monorepo (`SOURCE_REV` = `559751fd…`).
Path convention: paths beginning `xai-…` or `ptyctl…` are under `crates/codegen/`; other paths are repo-relative. "(inferred)" marks conclusions not stated directly in code or docs.

---

## 1. Identity

- **Language/runtime:** Rust (toolchain pinned to 1.94.0, `rust-toolchain.toml`), async on tokio. The agent itself runs on a **single-threaded tokio `LocalSet`** on its own OS thread (`xai-grok-pager/src/acp/spawn.rs:363,419`; `xai-grok-shell/src/agent/app.rs:119,265`), which is why session code uses `RefCell`/`spawn_local` freely (`acp_session.rs:1` allows `await_holding_refcell_ref`).
- **Distribution:** one native binary. The cargo artifact is `xai-grok-pager` (`xai-grok-pager-bin/Cargo.toml:14`); official installs rename it `grok`. Installed via `curl https://x.ai/cli/install.sh | bash` or a PowerShell script, and it has a self-updater (`xai-grok-update`, `grok update`, background auto-update in stdio mode, `xai-grok-pager-bin/src/main.rs:~1390`). Builds need DotSlash for a hermetic `bin/protoc` (README).
- **License:** Apache-2.0 for first-party code (`LICENSE`). Ported code keeps its original license: openai/codex under Apache-2.0 and sst/opencode under MIT (`xai-grok-tools/THIRD_PARTY_NOTICES.md`). A vendored Rust Mermaid stack lives in `third_party/`. External contributions are not accepted (`CONTRIBUTING.md`).
- **Packaging:** a Cargo workspace. The root `Cargo.toml` is generated (by Bazel tooling upstream; `Cargo.toml:469` mentions `bazel/lint/linters.bzl`) and lists 107 members: 90 crates in `crates/codegen/`, 12 in `crates/common/`, 1 in `crates/build/`, 1 in `prod/mc/`, and 4 in `third_party/`.
- **Size:** about 1.97M lines of `.rs` overall. `crates/codegen` is 1.91M, `crates/common` 60k, `third_party` 27k. Files named `*_test(s).rs` or under `tests/` add up to about 509k lines, and there are about 35.6k `#[test]`/`#[tokio::test]` annotations. Largest crates: pager TUI 551k, shell runtime 430k, tools 165k, workspace 125k. This is one of the largest open coding-agent harnesses.

## 2. Repository layout (crate map by layer)

**Binary / composition root**
- `xai-grok-pager-bin`: `main.rs` (3.8k LOC). Parses the CLI (TUI, `-p` headless, `agent stdio|serve|headless|leader`, `login`, `workspace`, `leader` management, `dashboard`, `worktree`, …) and wires the runtime.

**UI (clients)**
- `xai-grok-pager`: full-screen TUI on ratatui 0.29 and crossterm 0.28 (`Cargo.toml:241,157`). It uses an Elm-style Action→dispatch→Effect loop (`xai-grok-pager/README.md`). Also holds the headless driver, slash commands, and the `wrap`/`ssh` PTY wrappers.
- `xai-grok-pager-render`, `xai-grok-markdown` (streaming markdown), `xai-grok-mermaid` (Mermaid→PNG with no Node or browser), `xai-ratatui-inline`, `xai-ratatui-textarea`, `xai-grok-pager-minimal` (`--minimal` mode that writes into native scrollback), `xai-grok-pager-diff`, `xai-grok-status-line`, `xai-grok-voice` (dictation over xAI streaming STT), `xai-grok-gboom` (a `/gboom` raycaster easter egg drawn with kitty graphics).

**Agent runtime**
- `xai-grok-shell`: the agent runtime. It owns `MvpAgent` (the ACP agent), the per-session `SessionActor` (turn loop, tool calls, compaction, goals, memory capture, hooks, MCP), the leader IPC server, persistence, `x.ai/*` extension handlers, relay sync and trace upload.
- `xai-grok-agent`: `Agent`/`AgentBuilder`, agent-definition parsing (Markdown with YAML frontmatter), toolset presets, system-prompt templates, AGENTS.md and skills discovery, plugins (`src/plugins/`).
- `xai-chat-state`: conversation-state actor. `xai-grok-sampler`: HTTP streaming and retry actor. `xai-grok-sampling-types`: pure wire types (Chat Completions, Responses, Messages).
- `xai-grok-subagent-resolution`, `xai-agent-lifecycle`, `xai-prompt-queue`, `xai-interjection-core` and `xai-message-delivery-core` (common), `xai-workflow` (Rhai-scripted multi-agent workflows), `xai-grok-bundle` (cache of the xAI-published subagent/persona/skill bundle), `xai-grok-compaction` (common; shared with Grok chat), `xai-compaction-transcript`, `xai-token-estimation`.

**Tools**
- `xai-grok-tools`: all tool implementations and the registry. `xai-grok-tools-api` holds the protobuf tool API and config validation. Common crates `xai-tool-runtime`, `xai-tool-protocol` and `xai-tool-types` define the `Tool` trait. The `xai-computer-hub-*` crates (common) provide transport, registry and resolver for remote tool servers.
- `xai-codebase-graph` (tree-sitter definitions and references), `xai-fuzzy-file-search`, `xai-grok-shell-terminal` (local/ACP/PTY terminal runners), `ptyctl` (headless PTY on `alacritty_terminal`).

**Workspace / host**
- `xai-grok-workspace`: filesystem, VCS (git/jj), worktrees, checkpoints and rewind, the permission manager and the auto-mode classifier, per-command sandbox wiring, the hub ToolServer, and the `xai-workspace-server` binary.
- `xai-grok-workspace-client`, `-daemon` and `-types` are the in-progress daemon/client split. `xai-hunk-tracker` does agent-vs-external hunk attribution. `xai-fast-worktree` creates git worktrees with copy-on-write cloning. `xai-gix-status`, `xai-fsnotify`, `xai-file-utils`.

**Safety**
- `xai-grok-sandbox` (process-wide confinement through `nono` using Landlock/Seatbelt, seccomp for child network, per-command Seatbelt wrapping), `xai-grok-permission-rules` (rule engine and bash parsing), `xai-grok-egress-proxy` (exact-origin HTTP/CONNECT proxy), `xai-grok-secrets`, `xai-grok-extra-ca`.

**Integration**
- `xai-grok-mcp` (rmcp 2.1, quarantined together with reqwest 0.13, plus MCP OAuth), `xai-grok-hooks`, `xai-hooks-plugins-types`, `xai-grok-plugin-marketplace`, `xai-acp-lib`, `xai-grok-memory`, `xai-grok-session-search` (SQLite FTS5), `xai-grok-foreign-sessions` (lists other agents' sessions), `xai-grok-external-agent-migration` (imports Claude settings).

**Platform**
- `xai-grok-config` (layered TOML), `xai-grok-cloud-config` (signed remote settings), `xai-grok-login`/`-auth` (OAuth, OIDC, external auth commands), `xai-grok-telemetry` (Mixpanel, Sentry, OpenTelemetry), `xai-grok-otel`, `xai-mixpanel`, `xai-crash-handler`, `xai-grok-update`, `xai-grok-paths`/`xai-dirs`, `xai-sqlite-journal` (picks a journal mode that is safe on NFS).

**Test infrastructure**
- `xai-grok-test-support`, `xai-grok-pager-pty-harness`, `xai-test-utils`.

## 3. Process topology

Entrypoints are tagged in `xai-grok-pager-bin/src/main.rs:1376-1381`: `Stdio→Embedded`, `Leader`, `Serve→Workspace`, `Headless`.

1. **TUI (default `grok`).** By default the pager starts `MvpAgent` in the same process, on a dedicated OS thread with a current-thread runtime and a `LocalSet` (`xai-grok-pager/src/acp/spawn.rs:363-423`). Pager and agent talk **ACP (JSON-RPC) over in-memory channels** (`acp_channels`). The UI and the agent are therefore separated by protocol even inside one process.
2. **Headless (`grok -p`)** drives the same ACP agent and prints text, `json`, or `stream-json` output. The output schema mirrors Claude Code's SDK result subtypes such as `error_max_turns` (`docs/user-guide/14-headless-mode.md:295`).
3. **Agent mode** has several transports:
   - `grok agent stdio`: ACP over stdin/stdout, used by Zed, Neovim, Emacs and marimo.
   - `grok agent serve --bind … --secret …`: ACP over WebSocket with a shared-secret token.
   - `grok agent headless --grok-ws-url …`: connects outward to a WebSocket relay.
   - `grok agent leader`.
4. **Leader mode (optional, off by default).** One leader process per machine owns `MvpAgent` and its state. TUI, IDE and headless clients connect over a Unix socket (named pipe on Windows) using length-prefixed JSON frames of at most 64 MiB. The leader namespaces request ids and routes by session ownership (`xai-grok-shell/src/leader/mod.rs:1-33`, `transport.rs:1-6`, `protocol.rs:9`). `connect_or_spawn` starts a leader if none exists, and `LeaderReconnector` handles restarts, for example after an auto-update swaps the binary (`leader/protocol.rs:509`). Precedence is `--no-leader` > `--leader` > config `use_leader` > remote `leader_mode` > off. Any sandbox profile vetoes leader mode, so in-process tools stay confined (`xai-grok-pager/src/app/mod.rs:493-541`).
5. **Workspace server / Computer Hub (the in-progress split).** `xai-workspace-server` is a standalone daemon that double-forks and holds a pidfile lock (`xai-grok-workspace-daemon/src/lib.rs`). It connects outward to a hub over WebSocket and exposes workspace tools as a `ToolServer`, with sessions bound on demand through `session.bind` (`xai-grok-workspace/src/hub.rs:1-21`, `src/bin/workspace_server.rs:1-4`). `WorkspaceOps` has two modes: **Local**, which dispatches in-process, and **Proxy**, where "everything routes through hub WebSocket to a remote workspace server". Typed RPC structs are shared by client and server so the compiler checks both ends (`workspace_ops.rs:1-15`). This is how Grok runs in hosted cloud sandboxes: the agent talks to the model while tools execute on a remote box. `workspace_grok_build_toolset()` is the full tool list a hub-registered workspace serves (`xai-grok-agent/src/config.rs:~139`).
6. **Cloud services.** Inference defaults to `https://cli-chat-proxy.grok.com/v1` (shell README, env table). `relay/` syncs TUI sessions to a backend over WebSocket for cross-machine persistence and sharing (`xai-grok-shell/src/relay/mod.rs:1`). `upload/` sends per-turn trace artifacts to cloud storage (GCS), and remote settings arrive from `/v1/settings`.

## 4. Agent loop

**Core types.** `MvpAgent` (`xai-grok-shell/src/agent/mvp_agent/`) implements ACP. Each session is a `SessionActor` (`session/acp_session.rs:1-8`) that receives `SessionCommand`s (prompt, cancel, shutdown) through a channel and runs `run_session` (`acp_session_impl/run_loop.rs:1`). State is split into actors that communicate by messages instead of locks: `ChatStateActor` (conversation, `xai-chat-state/src/lib.rs`), `SamplerActor` (`xai-grok-sampler`), `HunkTrackerActor`, the persistence actor, and the signals actor.

**A turn end to end** (`session/acp_session_impl/turn.rs`):
1. `handle_prompt` → `handle_turn_input_inner` (`turn.rs:451,503`). The prompt is parsed for slash commands and skills (`prompt_parser::parse_prompt_with_skills`) and images are normalized. A prompt that is too large is offloaded to `prompts/prompt_{n}.txt`, and the model gets a bounded excerpt (`prompt_offload.rs:1-3`). Then the `UserPromptSubmit` hooks run and can block the prompt (`turn.rs:~1290`).
2. **Outer "round" loop** (`turn.rs:1313`) calls `process_conversation_turn_with_recovery` (`turn.rs:1977`). A completion requirement or structured-output JSON schema wraps it with retry. After each completed round:
   - Goal mode may inject a continuation (`run_goal_round_end`).
   - The **Stop hook gate** runs (`stop_gate.rs:236`). A `KeepWorking` verdict pushes `stop_hook_feedback` into the conversation and loops again.
3. **Inner sampling loop** in `process_conversation_turn` (`turn.rs:2564`, loop at `2747`). Each iteration:
   - Emits `LoopStarted`.
   - Checks **action stationarity**. A step signature is computed from every tool call, with JSON args canonicalized and the calls sorted so reordering parallel calls does not count as progress (`turn.rs:3946`). Thresholds (`turn.rs:3896-3900`):
     - Nudge after 4 identical calls for "problematic" kinds, or 8 otherwise.
     - Hard stop after 8 or 12 identical calls.
     - Pure no-op keepalives (`true`) end silently after 4.
   - Drains **interjections**: user messages typed mid-turn that are injected at safe points (`interjection.rs:303`). Also drains pending skill reminders and monitor events.
   - Adds a first-turn memory reminder and MCP-connecting reminders.
   - Starts **pre-fire two-pass compaction** when needed, or runs blocking auto-compaction when the threshold is crossed.
   - Adds a **long-reasoning reminder** when it is due (`long_reasoning_reminder.rs`).
   - Builds the request with the chat-state actor and samples through the sampler actor.
4. **Streaming.** The sampler has three layers (`xai-grok-sampler/src/lib.rs:1-10`):
   - L1 `SamplingClient` produces raw SSE chunk streams.
   - L2 `stream::{chat_completions,responses,messages}` turns them into `SamplingEvent`s.
   - L3 `SamplerHandle` adds retry and cancellation.
   Deltas are forwarded to the client as ACP `session/update` notifications (`agent_message_chunk`, thought chunks, `tool_call`, `tool_call_update`). `update_chunk_merge.rs` buffers chunks for replay.
5. **Tool dispatch** (`tool_calls.rs:335 execute_tool_calls`, `:484 execute_tool_calls_batch`). Calls are first **prepared one at a time**: argument parsing, `PreToolUse` hooks, the plan-mode edit gate, and a permission check through the workspace permission manager. If one call is rejected or cancelled, the calls after it get synthetic "cancelled due to earlier…" results. Approved calls are then **dispatched in parallel** through `FuturesUnordered` (`tool_calls.rs:1008`). Writers to the same path are serialized by per-path `tokio::Mutex` locks, derived from each tool's declared `lock_path_param` (`tool_calls.rs:~620-650`; the registry audits tools with path-like params that declare no lock path, `registry/types.rs:41-55`). Read-only calls are never locked.
6. **Stop conditions.**
   - No tool calls, and none of these apply: the TodoGate nudge (unfinished todos, at most 2 fires per prompt, `system_reminder.rs:6`), pending interjections, or a goal continuation.
   - `max_turns` reached (`turn.rs:~3850`, emits `max_turns_reached`).
   - Stationarity hard stop (`TurnOutcome::StationarityEnded`).
   - Cancellation, refusal, or a hook block.
   - `StopReason::Length` with no tool calls goes through **length salvage**: a continue reminder is injected and the sample retried within a budget (`length_salvage.rs`). Length-truncated tool-call samples are capped separately.
7. **Retries** (`xai-grok-sampler/src/retry.rs`):
   - `DEFAULT_MAX_RETRIES = 15`, `MAX_RETRY_BACKOFF = 30s` with jitter, and `Retry-After` is honored.
   - A rate-limit threshold applies, and transient retries have a time window (`sampler_turn.rs:32-86`).
   - Auth failures trigger re-auth and retry (`auth_retry.rs`).
   - Context-overflow errors trigger recovery compaction.
   - **Server-side doom-loop detection:** the inference server emits a `response.doom_loop_check` SSE event (`xai-grok-sampling-types/src/doom_loop.rs:33`). The client can abort the stream and retry with a fixed `<system_reminder>` that says the output was flagged as looping. The failed reasoning (8 KB cap) and text (4 KB cap) are replayed only when the turn made no tool call, so no reasoning item is left without its function call (`xai-grok-sampler/src/doom_loop_recovery.rs:1-28`).
8. **Turn end.** The loop flushes to disk, closes the file-state tracker's prompt window, persists a `RewindPoint` (`turn.rs:~1405`), records usage, runs turn-end hooks, and maps the outcome to an ACP `StopReason`.

**Goals (`/goal`).** A harness-driven autonomous mode spread across ~10 modules (`session/goal_*.rs`, `acp_session_impl/goal.rs`). `GoalTracker` is a pure state machine. A **planner** subagent writes `plan.md` and fails closed. A **strategist** fires after N failed verifications, fails open, and may not write `plan.md`. The guarantee is in the **verification**: an "adversarial skeptic panel" of N parallel subagents, each returning a JSON verdict and spawned directly so the parent transcript stays clean (`goal_classifier.rs:1-7`). A regex **premature-stop detector** catches phrases like "Stopping here for now" (`goal_stop_detector.rs`). An idle **laziness classifier** asks the model whether the session looks stalled: 10 s idle, confidence ≥ 0.7, transcript window of at least 30 items or 5 user and 5 assistant turns (`laziness_classifier.rs`). Goals can have a token budget.

## 5. Model/provider layer

- **Three wire protocols** in `ApiBackend` (`xai-grok-sampling-types/src/types.rs:1104`): `ChatCompletions` (the default for custom models), `Responses` (the xAI default), and `Messages` (Anthropic). Request-body caps are 50 MiB for xAI and 30 MB for Messages, `prompt_cache_key` is forwarded only on Responses, and only Chat Completions and Responses support native JSON schema. Structured output on Messages goes through a `StructuredOutput` tool. Messages models are not shown `use_tool`'s `oneOf` file forms because that API returns 400 on them (`sampler_turn.rs:443-446`).
- **Default models** come from embedded `xai-grok-models/default_models.json`: `grok-4.6` and `grok-4.5`, each with a 500k-token context window, `api_backend: "responses"`, reasoning efforts low through xhigh (default `high`), `auto_compact_threshold_percent: 80`, `compaction_at_tokens`, and `compactions_remaining: 1`. Resolution order is CLI flag > env > config.toml > remote settings > JSON (`xai-grok-models/src/lib.rs`).
- **Compaction headers.** The sampler can send `x-compactions-remaining` and `x-compaction-at` per model (`xai-grok-sampler/src/config.rs:109-115`). (inferred) The server or model is told its compaction budget, which suggests the model is trained to expect compaction.
- **Other providers:** BYOK custom endpoints, Ollama, OpenAI-compatible servers, and `GROK_MODELS_BASE_URL` with automatic `/models` discovery (`docs/user-guide/11-custom-models.md`). Enterprise `requirements.toml` can pin a model allowlist.
- **Auth** (`xai-grok-login`):
  - Browser OAuth to grok.com (tokens in `~/.grok/auth.json`, 7-day expiry) and device-code flow.
  - `XAI_API_KEY`.
  - **OIDC with PKCE against a customer IdP**, with the bearer token sent to a customer proxy.
  - An **external auth provider command**: it writes the login UI to stderr and the token (bare or JSON with expiry) to stdout, refreshed 5 minutes before expiry.
  - Per-model auth providers, and single-flight refresh (`single_flight.rs`).
  - Auth is a dependency-inversion seam in `xai-grok-auth`.
- **Reasoning.** Reasoning effort is applied only when the model supports it (`mvp_agent/reasoning_effort.rs`). Responses `encrypted_content` reasoning items are kept with their function calls on replay (`doom_loop_recovery.rs:7-11`). Side calls (title, recap, `/btw`, classifier, compaction) reuse cache-aligned request prefixes (`acp_session_impl/side_call.rs`, `recap.rs:3`). A `SamplerHandle` is prewarmed at session creation (`sampler_prewarm.rs`).

## 6. Tool system

**Definition and registration.** Tools implement the `Tool` trait from `xai-tool-runtime` ("unified runtime contract": `Tool`, `ToolDispatch`, `ToolStream`, `ToolError`; `crates/common/xai-tool-runtime/src/lib.rs`). Metadata comes from `ToolMetadata`: `ToolKind`, `ToolNamespace`, `lock_path_param`, description template (`xai-grok-tools/src/types/tool.rs`).
- `ToolRegistryBuilder::finalize()` produces a `FinalizedToolset` behind a `parking_lot::RwLock`. Reads are microsecond lookups and writes are rare, for example MCP registration (`registry/types.rs:434`).
- Out-of-tree "tool packs" self-register through a process-global `register_tool_pack` (`registry/types.rs:27-38`). Named presets can be registered externally (`xai-grok-agent/src/config.rs:37`).
- Clients send a `ToolServerConfig` listing tool ids such as `"GrokBuild:read_file"`, per-tool params, **renames of tools and params**, and kinds. Schemas are generated with `schemars` and memoized by `TypeId` (`registry/types.rs:2123`).
- Tool descriptions and prompts are **MiniJinja templates** that use `${{ tools.by_kind.read }}` and `${{ params.edit.old_string }}`, so prompts follow renames and alternate namespaces instead of hardcoding names (`xai-grok-agent/src/prompt/subagent_prompts.rs:1-19`).
- Tools also carry **post-execution `Reminder`s**: per-tool ones (empty file, offset past end) and cross-cutting ones (skill discovery) (`types/tool.rs:~120`).
- Version-managed tools have behavior presets (`"current"`, `"legacy-0.4.10"`) so older contracts can be kept (`versions.rs`, `search_replace/versions/`).

**Dialects (`ToolNamespace`, `types/tool.rs:29`):** `GrokBuild`, `GrokBuildConcise`, `GrokBuildHashline`, `Codex`, `OpenCode`, `MCP`. The code also refers to an external `Cursor:*` namespace (`Cursor:Shell` and `Cursor:Task`, `xai-grok-agent/src/builder.rs:181,954`) and to a stubbed `cursor_worker` leader feature (`leader/cursor_worker_stub.rs`). The Cursor tools are not in this tree.
- **Named presets** (`xai-grok-agent/src/config.rs:199`): `grok-build`, `grok-build-concise`, `grok-build-plan`, `codex`, `explore`, `plan`, `grok-computer`, plus an orchestrator toolset.
- **`codex`** (`config.rs:340`) uses faithful ports of codex-rs `read_file`, `apply_patch`, `list_dir` and `grep_files`, and has its own system prompt (`templates/apply_patch_prompt.md`, chosen by `TemplateOverride::Codex`, `prompt/context.rs:17`).
- **OpenCode ports** (bash, edit, glob, grep, read, skill, todowrite, write) keep opencode's camelCase params. In the default toolsets only `OpenCodeWriteTool` is used.
- **`grok-build-concise`** returns compact line numbers and shorter messages.
- **Why dialects exist** (inferred): models trained or evaluated against other harnesses' tool formats can run inside Grok Build. Subagent resolution supports this: `harness_agent_type` lets "a grok-build session run an alternate-harness verifier and vice-versa" (`xai-grok-shell/src/agent/subagent/mod.rs:1638`). The file-toolset choice can also come from **remote settings** (`tools/config.rs: resolve_file_toolset`), which implies server-driven A/B tests.

**Hashline edit format** (`implementations/grok_build_hashline/`). Selected with `[toolset] file_toolset = "hashline"` (`xai-grok-shell/src/tools/config.rs:169,325`). The registry rejects mixing it with the standard bundle (`registry/types.rs:965`). It provides three tools: `hashline_read`, `hashline_edit` and `hashline_grep`.
- `hashline_read` prefixes every line with an **anchor** made from the line number and short hashes, in the form `LINE:LOCAL[:CTX]→content` (`edit/apply.rs:110`).
- `LOCAL` is an FNV-1a hash of the line with whitespace normalized: trimmed, internal runs collapsed, token boundaries kept. It is encoded as 1–4 lowercase letters, 3 by default (`util/hash.rs:34-60`).
- Three schemes plug in through the `AnchorScheme` trait (`scheme.rs:1-17`):
  - **A `ContentOnly`** `22:abc`: validates only the line's own content.
  - **B `ChunkFingerprint`** `22:abc:rst`: adds a fingerprint of the fixed-size chunk containing the line, so an edit invalidates only anchors in that chunk. This is the default; `HashlineSchemeParams` default `chunk_size = 8`, `config.rs:20-40`.
  - **C `CheckpointChain`**: adds a hash chained from the nearest preceding checkpoint, which detects staleness most strongly but changes more anchors after each edit.
- `hashline_edit` takes `{file_path, edits:[…]}` with ops tagged by `"op"` (`edit/types.rs:47`):
  - `replace {anchor, end_anchor?, content}`: replaces a line or range; empty content deletes it.
  - `insert_after {anchor | "0:" | "EOF", content}`.
  - `write {content}`: replaces the whole file.
- Every anchor is validated against the **pre-edit snapshot** before anything is applied. Overlapping edits are rejected, and edits are applied bottom-up so line shifts do not interfere (`edit/mod.rs:1-6`, `apply.rs:150`).
- Stale anchors are recovered by searching ±15 lines for a matching local hash (`scheme.rs:173,527`). A hash without a line number resolves only if exactly one line matches (`apply.rs:505`).
- A successful edit returns **fresh anchors** for the edited region, so the model can chain edits without re-reading. Failures return structured error kinds.
- A range-size policy warns on medium (6–20 lines) and large (>20 lines) replacements (`edit/range_policy.rs`).
- The deserializer accepts `edits` as a double-encoded JSON string or as a single object, both common model mistakes (`edit/types.rs:19-43`).
- An offline benchmark compares the schemes with microbenchmarks and deterministic edit-trace simulation (`benchmark.rs`).
- Purpose: the model points at lines by short content-verified handles instead of reproducing `old_string` exactly. This saves output tokens, removes the "old_string not unique or not found" failure class, and still catches edits against stale views.

**Built-in tools.** The core grok-build set (`config.rs:258-286`) is:
- `run_terminal_command` (bash, with background mode), `read_file`, `search_replace`, `list_dir`, `grep`.
- `kill_command_or_subagent`, `todo_write`, `get_command_or_subagent_output`, `wait_commands_or_subagents`, `spawn_subagent`.
- `scheduler_create/delete/list` and `monitor`.
- `search_tool` and `use_tool`.
- `update_goal` and `workflow`.

The presets show that model-facing names are renamed from the internal ids (for example `task`→`spawn_subagent` and `run_in_background`→`background`, `config.rs:119-136`). Additional tools: `web_search` (server-side "backend search" when the model supports it), `web_fetch` (off by default, domain allowlist), `enter/exit_plan_mode`, `ask_user_question`, `memory_search/get`, `lsp`, `image_gen`, `video_gen`/`image_to_video`/`reference_to_video`, `send_subagent_message`, `send_feedback`. `read_file` also handles images, PDF and PPTX (`implementations/read_file/`).

**MCP tools are deferred.** Only built-ins go in the tool list ("Local mode: tool search is always enabled", `sampler_turn.rs:432`). The model finds MCP tools with **BM25 `search_tool`**, using an index rebuilt per search (`session/tool_index.rs:1`), and calls them through `use_tool`.

**Output truncation:**
- Default cap is 40,000 bytes for tool output and 20,000 characters for bash (`xai-grok-tools/src/lib.rs:9-13`).
- Bash keeps the beginning and end of long output, writes everything to a log file, and returns its path (`grok_build/bash/mod.rs:368,1513`).
- MCP output has its own cap (`MCP_MAX_OUTPUT_BYTES`, env-overridable).
- Oversized media-generation batches are rejected with a resample reminder.

## 7. Context engineering

- **System prompt.** Three templates: `templates/prompt.md` (119 lines, main), `subagent_prompt.md` and `apply_patch_prompt.md`. They are MiniJinja with `${{ }}`/`${% %}` delimiters and conditional sections: interactive vs non-interactive, memory v2 paths, `tools.by_kind.*`, scratch dir, browser verification. At build time they are **XOR-obfuscated into `prompt_encrypted.rs`** and decrypted on demand into `Zeroizing<String>`. The code calls this "obfuscation, not security" (`xai-grok-agent/src/prompt/template.rs:1-3`); the plaintext also ships in this repo. `PromptContext` is serializable for inspection (`grok prompt --section …`). `PromptAudience::{Primary, Subagent}` picks the template. After compaction a minimal `COMPACT_SYSTEM_PROMPT` is used (`template.rs:~50`).
- **Instruction files.** Discovery walks from the cwd up to the repo root plus `~/.grok/` and reads `AGENTS.md` and `CLAUDE.md`. It also scans rules directories `.grok/rules/`, `.claude/rules/`, `.cursor/rules/`, home-scope `~/.{grok,claude,cursor}/rules/`, and `[paths] extra_rule_dirs` (`prompt/agents_md.rs:1-8`). Cursor rules are applied on read (`implementations/cursor_rules_on_read.rs`). `grok inspect` lists every discovered source with token counts.
- **System reminders.** Harness messages wrapped in `<system-reminder>`-style tags, with untrusted content escaped (`acp_session_impl/reminders.rs:3-17`). Policies cover the TodoWrite nudge and TodoGate, date rollover, workflow status and completion, MCP connecting or failed, skill discovery, output-limit continue, action-stationarity nudge, memory first-turn, and long-reasoning. A comment in `long_reasoning_reminder.rs:1` records that reminder placement was tuned per model: "made grok-4.7 reason more…".
- **Compaction.**
  - The shared engine `crates/common/xai-grok-compaction` has three styles. Grok Build uses **`code_compaction`**, which summarizes the whole session and replaces it; Grok chat uses the intra and inter styles. The split point never separates a tool request from its results.
  - The default threshold is 85% (`xai-grok-agent/src/compaction.rs:28`), overridden per model to 80%.
  - Options include a compaction model, a 300 s wall-clock budget, and an optional **memory flush turn** before compacting.
  - Optional **two-pass "prefire"**: pass 1 summarizes about 95% of history in the background before the threshold. At compaction, pass 2 combines that note with the about 5% tail (`session/two_pass.rs:1-5`).
  - Compacted segments are rendered to Markdown transcripts on disk (`xai-compaction-transcript`, `compaction_segments.rs`) and kept in `compaction_checkpoints/`.
  - Triggers: manual `/compact`, pre-sampling auto-compaction, preflight overflow detection, error-recovery compaction, and compaction on model switch (`session/compaction.rs:1-3`). `PreCompact` and `PostCompact` hooks fire.
- **Memory** (`xai-grok-memory`, opt-in through `GROK_MEMORY`/`[memory]`) has two isolated pipelines:
  - **Legacy:** `~/.grok/memory/MEMORY.md` plus per-workspace (blake3 of the cwd) MEMORY.md and session logs.
  - **v2:** `~/.grok/memory-v2/` with `topics/`, `observations/_inbox/`, and a generated read-only `MEMORY.md`. Writes are policy-limited to `.md` files in those two directories.
  - Search is hybrid: SQLite FTS5 BM25 + sqlite-vec KNN, temporal decay, source weights, and MMR (`search.rs:1-18`).
  - A **"dream"** consolidation runs on time and session-count gates, with batch variants (`dream.rs`, `batch_dream*`). `/flush` captures knowledge explicitly.
- **Other context inputs:** a `repo_status_prefix` (git state), image normalization and description, and a **file-acceleration** path for local sessions that are not confined by a sandbox (`file_acceleration.rs:1-5`).

## 8. Sessions and state

- **Storage.** Plain files under `~/.grok/sessions/<url-encoded-cwd>/<session-id>/` (shell README, "Session Persistence"):
  - `summary.json` (index).
  - `updates.jsonl` (the ACP update stream, which drives transcript replay on `/load` and which code comments call the source of truth).
  - `chat_history.jsonl` (the raw model conversation). Loading reads this file directly and rebuilds it from `updates.jsonl` only when it is missing or empty (`session/storage/jsonl/mod.rs:217-225`).
  - `plan.json`, `rewind_points.jsonl`, `signals.json`, `feedback.jsonl`, `events.jsonl` (typed per-session events, `xai-grok-session-events`), `compaction_checkpoints/`, `subagents/`, `prompts/`.
- All writes go through one **persistence actor** with a documented power-loss contract. Fire-and-forget appends may lose the current turn's tail. Awaited barriers (`FlushAndAck`) fsync only the files dirtied since the last barrier. Atomic renames fsync the temp file and, for creates, the directory (`session/persistence.rs:1-14`).
- A `chat-history-downgrade` binary exists for format rollback (`xai-grok-shell/Cargo.toml:352`). SQLite is used for search (FTS5), memory and the dashboard, with a journal mode chosen to be safe on NFS (`xai-sqlite-journal`).
- **Resume:** `-c` (most recent in the cwd), `-r <id>`, `-s <name>` (create or resume a named session), ACP `session/load` and `session/resume`, `/resume` picker, and `fork` to a new cwd (`session/fork.rs`). Remote sessions are pulled on miss (`remote/pull.rs`), and listings merge local and remote (`session/merge.rs`). Sessions from other agents are listed metadata-only (`xai-grok-foreign-sessions`) and can be imported from Claude Code (`claude_import.rs`).
- **Checkpoints and rewind.** At each prompt boundary a `RewindCheckpoint` keyed by `prompt_index` bundles a filesystem `RewindPoint`, an optional hunk delta, and optional git HEAD/index; restore reverts all three together (`xai-grok-workspace/src/session/checkpoint.rs:1-6`). `RewindMode::{All, ConversationOnly, FilesOnly}` (`xai-grok-shell/src/session/acp_types.rs:323`), exposed as `x.ai/rewind/*` (`extensions/rewind.rs`). A durable mirror under `<cwd>/.grok/rewind-checkpoints/<session>/checkpoint-N.json` is off by default (`GROK_WORKSPACE_REWIND_DURABLE`) and is not yet used for restoring (`checkpoint_store.rs:1-12`). The `RewindPoint` is written at turn end, so a turn killed midway likely cannot be rewound. Rewind can cross compactions (`acp_session_impl/rewind.rs`).
- **Hunk tracker** (`xai-hunk-tracker`): an actor that attributes every file hunk to the agent or to an external editor, exposed through `extensions/hunk_tracker.rs`. It feeds diff review and the rewind deltas.
- **Subagent attempts.** A durable attempt store with intent, completion, accounting and recovery codecs exists in `agent/subagent/attempt_store/`, but nothing calls it yet (`attempt_store/mod.rs:1-22`). On load, subagents left running are marked cancelled, "interrupted by process restart" (`agent/subagent/mod.rs:2534-2640`).

## 9. Permissions, safety, sandboxing

**Authorization pipeline** (`docs/user-guide/22-permissions-and-safety.md:119-138`):
1. `PreToolUse` hooks, which can only deny.
2. Rules: deny > ask > allow, from CLI `--allow/--deny`, TOML, or Claude `settings.json`.
3. Remembered per-project grants.
4. Built-in read-only auto-approvals.
5. The mode's prompt policy.

Modes are `default` (ask), `acceptEdits`, `plan`, `auto`, `dontAsk` and `bypassPermissions` ("always-approve"/yolo). Administrators can lock out always-approve through `requirements.toml`. Bash commands are parsed with tree-sitter into segments with wrappers removed (`xai-grok-permission-rules/src/bash_command_splitting.rs`), and every segment of a compound command must pass.

**Auto-mode classifier** (`xai-grok-workspace/src/permission/auto_mode/mod.rs`, 3.4k LOC; "Port of common agent auto-permission classifier semantics"):
1. **Fast path** (`:1085`). Tools needing user interaction always prompt. Allowlisted reads, greps and tools are allowed. **All edits are allowed** ("accept-all-edits product decision", regardless of path). Typed agent messages and the exact no-ops `true`, `:` and `false` are allowed. Everything else goes to `Classify`.
2. **Deterministic heuristic pre-pass** (`HeuristicPermissionClassifier::classify_sync`, `:356-480`):
   - Blocks dangerous substrings (`rm -rf /`, `curl|sh`, `/dev/tcp/`, `base64 -d`, …) and hostile-intent phrases in the transcript.
   - Allows routine prefixes on word boundaries.
   - Package managers go through a fail-closed allowlist of safe subcommands, and git through a fail-closed `routine_git` arm.
   - `rg --pre` and unsafe kubectl flags are guarded, writes to arbitrary destinations are detected, and env-var risk is tiered.
   - A provable heuristic **Allow skips the LLM** unless static analysis produced **security findings**.
3. **LLM side query** (`LlmPermissionClassifier::classify`, `:1433`). The session's own sampler is used through a channel. The request carries:
   - A system prompt (`templates/auto_mode_classifier_system_prompt.md`) defining **hard-wait** actions, which nothing can authorize (exfiltration, production mutation, ssh or another machine, credential access, irreversible deletion, untrusted code), and **clearable** actions, which only the user's live request authorizes (force push to their own branch, `git reset --hard`, …), with stacked-PR rules.
   - Trusted security findings as fixed enum tokens, so an attacker cannot inject text through them (`security_findings.rs:1-4`).
   - Harness-recorded prior decisions as inert single-line JSON.
   - AGENTS.md, marked **untrusted** with Markdown headings neutralized.
   - A trailing user message holding the recent transcript and the proposed action.
   - A strict JSON schema `{thinking, shouldBlock, reason}` (`:1119-1141`, `build_classifier_messages :1182`).
   Prompt variants (`Full`, `NoUserToolPrefix`, `BareInstructions`, `JustCommand`) trade context for cost.
4. Unparseable output falls back to the heuristic, except when findings exist, in which case it fails closed (`:1485-1495`).
5. **Manager handling** (`permission/manager/mod.rs:1040-1080`). A Block denies and is reported to the model. After a limit of consecutive or total denials, the user is prompted. Timeouts and unavailability also escalate to the user. In non-interactive runs, an escalation becomes a failure reported to the model.

**Sandbox** (`xai-grok-sandbox`). There are two separate mechanisms:
- **Process-wide** (`src/lib.rs:1-8`). The `nono` crate applies **Landlock (Linux ≥ 5.13)** or **Seatbelt (macOS)** to the whole `grok` process once at startup, and it cannot be undone. Built-in in-process tools are covered too, and children inherit the confinement. **Child network** is blocked with a seccomp filter in `pre_exec` (`child_net.rs`). The process itself keeps network access for the LLM API, so in-process web tools are not restricted.
  - Profiles: `off` (default), `workspace`, `read-only`, `strict`, `devbox`, plus custom `sandbox.toml` profiles with `extends`, `read_only`, `read_write`, `deny` and `restrict_network`. `~/.ssh`, `~/.aws`, `~/.gnupg` and `~/.grok/auth` are always write-protected.
  - If the sandbox cannot be applied, grok logs a warning and **continues unconfined** (shell README, Sandbox, Current Limitations). Violations are logged to `~/.grok/sandbox-events.jsonl`.
- **Per-command** (`src/command/mod.rs:1-11`), used by the workspace daemon. Each shell command is wrapped by a backend; today the only one is macOS Seatbelt through `sandbox-exec`. On a host with no backend, `Enforce` refuses to run commands; there is never a silent fallback to unsandboxed execution.
  - Protected paths are rendered last and can never be granted. They include git config and hooks, followed through symlinks (`command/git_config.rs`).
  - A violation decoder turns a denial into a grantable `Violation`, and an expiring grant store widens the next policy. The workspace replays the command once a grant is given (`xai-grok-workspace/src/sandbox/mod.rs`).
  - Network holds go through a per-folder proxy decider, built on `xai-grok-egress-proxy` with exact-origin allow policy.

Folder trust gates project hooks, plugins and MCP (`folder_trust.rs`, `xai-grok-hooks/src/trust.rs`).

## 10. Extensibility

- **Hooks** (`xai-grok-hooks`):
  - Events (`src/event.rs:79-260`): `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`, `Stop`, `StopFailure`, `StopCancelled`, `Notification`, `SubagentStart`, `SubagentStop`, `SubagentEnd`, `PreCompact`, `PostCompact`, `SessionEnd`.
  - Aliases accept Claude Code PascalCase and **Cursor hook names** (`beforeShellExecution`, `afterFileEdit`, `afterAgentResponse`, …).
  - Two runners: `command` and `http` (`src/runner/`). Hooks come from `.grok/hooks/`, config files and plugins, with matchers, gate semantics, and some events forwarded to the hub.
  - Stop and SubagentStop can keep the agent working (§4).
- **Plugins and marketplace** (`xai-grok-agent/src/plugins/`, `xai-grok-plugin-marketplace`). Plugins may provide skills, commands, agents, hooks (`hooks/hooks.json`), MCP (`.mcp.json`) and LSP (`.lsp.json`). The **Claude Code plugin layout** is supported, including `~/.claude/plugins/installed_plugins.json` and `known_marketplaces.json`. Install methods are git install and a local refresh, and plugins pass through a trust check. Plugin agents cannot declare MCP servers or hooks, or set bypassPermissions (`docs/user-guide/16-subagents.md:~240`).
- **MCP** (`xai-grok-mcp`). Uses rmcp 2.1 (quarantined because it needs reqwest 0.13), stdio and HTTP transports, and MCP OAuth with a credential store and elicitation.
  - Events are coalesced in a 50 ms window and pushed to clients as `x.ai/mcp/server_status`.
  - Stdio servers restart automatically (`session/mcp_dispatcher.rs:1-24`).
  - A managed connector catalog comes from the gateway (`GET /v1/mcp/tools/list`, `session/managed_mcp.rs`).
  - Config sources: `config.toml`, `.grok/config.toml`, `~/.claude.json`, `.mcp.json`, and the ACP client's `mcpServers`.
  - Tools are exposed only through `search_tool`/`use_tool` (§6). Subagents inherit connected servers, controlled by `mcpInheritance: all|none|named|except`.
- **Subagents.**
  - `spawn_subagent` (background flag, `resume_from` to continue a finished child's transcript) and `send_subagent_message` for parent↔child messaging and interjection (`parent_interject.rs`).
  - Built-in types `general-purpose`, `explore` and `plan`. The explore toolset enforces read-only at the tool level, not just in the prompt (`config.rs:356`). Custom agents are Markdown files with frontmatter in `.grok/agents/` and `~/.grok/agents/`; `.claude/agents/` is also read.
  - **Personas and roles** follow the precedence override > role > persona > parent (`xai-grok-subagent-resolution`). Capability modes are `read-only`, `read-write`, `execute` and `all`.
  - **`isolation: worktree`** gives a child its own git worktree, created quickly with CoW cloning (`xai-fast-worktree`) and merged back through `x.ai/git/worktree/apply`.
  - Depth is limited to 1. The bundled roles, personas and agents are downloaded from xAI into `~/.grok/bundled/` with per-file checksums so user edits are not overwritten (`xai-grok-bundle`).
- **Workflows** (`xai-workflow`). **Rhai scripts** (`.grok/workflows/*.rhai`, `~/.grok/workflows/`) orchestrate agents through a host channel with `agent()`, `parallel()` and `budget()`.
  - Limits: agent budget default 128 and maximum 1024, `MAX_PARALLEL = 1024`, `MAX_HOST_CALLS = 10000`, and a host-side concurrency cap of 32 (`xai-workflow/src/lib.rs`).
  - Workflows have a journal and pause/resume, which is not exactly-once (`04-slash-commands.md:~300`).
  - Built-in workflows include `/deep-research`, which verifies claims through independent verifier shards. Goals can run on the workflow engine.
- **Skills.** `SKILL.md` folders in `.grok/skills/`, `~/.grok/skills/`, `.claude/skills/`, bundled, plugin and config paths. They are discovered for injection into the system prompt (`prompt/skills.rs:1-4`) and loaded on demand. grok.com product skills are fetched over REST (`remote/skills_client.rs`). Custom slash commands come from plugin and Claude `commands/`.
- **Other:** `/loop` and the scheduler (cron-like recurring prompts with a 7-day expiry), `monitor` (watch scripts whose events are injected into the turn), LSP tool and servers (`lsp.json`), and `/btw` side questions.

## 11. Clients and UI

- **TUI.** ratatui with crossterm, in an Elm-like design: `AppView` → `AgentView` per session; input → `Action` → `dispatch` → `Effect` (`xai-grok-pager/README.md`). The name **"pager"** reflects the core concept: a scrollback-centric viewer of the conversation built on a "v3 pager rendering engine" (`src/lib.rs:8-10`). Features:
  - Streaming Markdown rendering, Mermaid diagrams rasterized in-process (vendored dagre/graphlib ports) and shown with terminal graphics.
  - Inline media through ffmpeg, an external editor, mouse support, themes (`pager.toml`), and a status line configurable by script (`[ui.status_line]`).
  - Tasks pane (Ctrl+G), plan approval view, `/workflow runs` dashboard, and a multi-session **Agent Dashboard** backed by SQLite (`xai-grok-dashboard-store`).
  - `--minimal` mode prints finished blocks into the terminal's **native scrollback** (`xai-grok-pager-minimal`).
- **ACP / editor integration.** `grok agent stdio` is used by Zed, Neovim (CodeCompanion, avante), Emacs agent-shell and marimo; JetBrains is listed as "coming soon". Grok adds more than 30 `x.ai/*` extension methods and notifications covering fs, git, worktree, fuzzy search, terminals, rewind, compaction, auth and feedback (`docs/user-guide/15-agent-mode.md:142-176`). The TUI uses the same ACP surface, so editors get feature parity.
- **Voice.** Push-to-talk dictation: microphone audio streams to xAI STT and the transcript lands in the prompt box (`xai-grok-voice/src/lib.rs:1-4`).
- **Other:** `grok wrap` and `grok ssh` run commands in a local PTY and intercept OSC 52 clipboard sequences for remote shells (`xai-grok-pager/src/wrap_cmd.rs`). `grok clone` mounts a "Grove" projected working tree over FUSE or NFS (`docs/user-guide/27-grok-clone.md`). The CLI also has `grok inspect`, `grok sessions`, `grok mcp`, `grok memory` and `grok usage`.

## 12. Configuration

- **TOML, layered** (`xai-grok-config/src/lib.rs:4-13`). Lowest to highest priority:
  1. `/etc/grok/managed_config.toml`
  2. `$GROK_HOME/managed_config.toml`
  3. `$GROK_HOME/config.toml`
  4. Cloud `requirements.toml`, an Ed25519-signed cache
  5. `/etc/grok/requirements.toml`
  6. macOS MDM (`ai.x.grok`)
  Each layer can apply `[[version_overrides]]`, and requirements layers can fail closed at startup.
- Project `.grok/config.toml` adds MCP servers, permissions and hooks. `.grok/sandbox.toml` and `.grok/lsp.json` are also read. **Claude Code settings are read as a fallback** (`.claude/settings.json`), and `/import-claude` migrates them (`xai-grok-external-agent-migration`).
- Remote settings come from `/v1/settings`, are cached signed, and are refreshed mid-session (`xai-grok-cloud-config`). They drive feature rollouts such as leader mode, file toolset, length-salvage budget and laziness thresholds.
- Config hot-reloads through a watcher (`xai-grok-shell/src/config/watcher.rs`, `reloader.rs`).
- Many `GROK_*` env vars (shell README env table). Model catalog from `default_models.json`, overridable per model with `[model.<name>]`.

## 13. Testing and quality infrastructure

- **Scale:** about 35.6k test functions and about 509k LOC of dedicated test files; large modules have sibling `*_tests.rs` files.
- **ACP harness:** `xai-grok-shell/tests/acp_harness/` is an ACP-level end-to-end harness with scripted clients, conversation replay, wire transcripts and policy fixtures. It runs against a mock inference server (`xai-grok-test-support/src/`: `acp_scripted_client`, `conversation_replay`, `counting_server`, …).
- **PTY harness:** `xai-grok-pager-pty-harness` runs the real binary in a PTY and reads the virtual screen through `alacritty_terminal`. Per-frame timing comes from DEC 2026 synchronized-update markers, and content comes from a mock inference server. It is used for YAML regression scenarios, benchmarks with baseline comparison, and a scroll-validation matrix (`src/lib.rs:1-20`). There is also an in-process **leader-cluster** test that runs several real `AppView`s against a real leader and `MvpAgent` (`xai-grok-pager/src/app/leader_cluster/`).
- **Other:** dhat heap-soak tests (`mvp_agent/tests/dhat_soak.rs`), criterion benches, live-backend smoke tests marked `#[ignore]`, and a hashline scheme benchmark.
- **Lints:** clippy config, `#![deny(clippy::indexing_slicing)]` in nearly every crate, `#![forbid(unsafe_code)]` in hub core, and "drift tests" that keep wire vocabularies in sync across crates (`auto_mode/mod.rs:28-49`). Many crates also carry a blanket `#![allow(unused_imports, dead_code, …)]`, (inferred) a side effect of syncing a subset of the monorepo.
- **CI is not part of the mirror:** there is no `.github`, and the root `Cargo.toml` is generated by Bazel upstream.

## 14. Distinctive features

1. **Multiple tool dialects in one runtime.** Grok-native, concise, **hashline**, faithful **codex** ports (with a codex-specific prompt), **opencode** ports, and hooks for external `Cursor:*` tools, all selectable per session or subagent and even by remote setting. The intent appears to be running models in the tool format they were trained or evaluated with (inferred).
2. **Hashline anchored editing.** Content-hash line anchors with a pluggable freshness scheme, bottom-up atomic multi-edit, ±15-line drift recovery, and fresh anchors returned after each edit. This replaces exact-string `old_string` matching.
3. **Auto-mode permission classifier.** A layered fail-closed design: fast path, deterministic tree-sitter bash heuristic, then an LLM side query that treats AGENTS.md and transcript text as untrusted, takes only enum-tokenized static findings as trusted, and returns a strict JSON verdict, with escalation to the user on denials or timeouts.
4. **Server-cooperative loop control.** Doom-loop signals come over SSE from the inference server, `x-compactions-remaining`/`x-compaction-at` headers give the model a compaction budget, and prompt-cache keys and side calls align with cached prefixes. The harness and the model are co-designed.
5. **Harness-owned anti-laziness machinery.** Action-stationarity detection over canonicalized step signatures, the TodoGate, length salvage, a regex premature-stop detector, an idle LLM laziness classifier, and goal mode with **adversarial skeptic-panel verification** that runs in subagents hidden from the parent transcript.
6. **ACP everywhere.** Even the built-in TUI talks ACP to an in-process agent. An optional **leader** process lets several clients (TUI, IDE, headless) share one agent per machine over a Unix socket.
7. **Hub/ToolServer split** for remote sandboxes. The same typed `WorkspaceOps` RPC runs locally or proxied to `xai-workspace-server`, so model I/O and tool execution can live on different machines.
8. **OS-kernel sandbox of the whole agent process** (Landlock/Seatbelt through `nono`, seccomp for child network), irreversible once applied, plus a separate per-command Seatbelt backend whose denials become grantable, expiring policy widenings.
9. **Rhai-scripted workflows** with agent budgets, parallel panels, a journal and pause/resume, used to build features like `/deep-research` and goal verification.
10. **Deep compatibility with Claude Code and Cursor:** CLAUDE.md, `.claude/{skills,agents,plugins,rules,settings.json}`, `.mcp.json`, Claude plugin marketplaces, Cursor rules, Claude and Cursor hook names, Claude-shaped headless stream-json, and session import.
11. **Durable, auditable state engineering:** an fsync-barrier persistence actor with an explicit loss contract, a WAL/rollback journal choice that works on NFS, checksum-tracked bundle caches, hunk attribution, and three-domain rewind (files, hunks, git).
12. **Two-pass prefire compaction**, with the first summary pass computed in the background before the threshold is reached.

## 15. Strengths, weaknesses, lessons

**Strengths**
- Actor decomposition (chat state, sampler, persistence, hunk tracker, signals) keeps the large session logic mostly lock-free. The single-threaded `LocalSet` turns many concurrency bugs into compile-time or panic-visible borrow errors.
- One protocol (ACP plus `x.ai/*` extensions) serves the TUI, editors, SDKs, the leader and the relay, which avoids divergent client paths.
- Tool prompts are templates keyed on `ToolKind`, so renames and dialect swaps never break prompt text. Per-path write locks give safe parallel tool execution.
- The safety design is careful: fail-closed defaults, an explicit trusted vs untrusted split in classifier prompts, a sandbox that cannot be relaxed at runtime, and a per-command wrapper that never falls back to an unwrapped launch.
- Testing investment is very large: PTY screen scraping with frame timing, ACP replay against mock inference, and leader-cluster tests.

**Weaknesses and costs**
- **Size and complexity.** About 1.9M LOC, with 4–5k-line session files (`turn.rs` 4.5k, `tool_calls.rs` 3.9k, `permission/manager/mod.rs` 7.8k). The goal subsystem alone is about 10 modules. Much of this is feature-flag and remote-setting branching, so readers and contributors face a steep curve. The public mirror omits the referenced `crates/codegen/AGENTS.md`, CI and Bazel.
- **Closed-loop dependencies.** Many behaviors assume xAI infrastructure: cli-chat-proxy, server-side doom-loop events, compaction headers, remote settings, the bundle server, relay, GCS trace upload and Mixpanel telemetry. BYOK providers get a reduced experience (inferred).
- **Partial sandbox.** Off by default. If it cannot be applied, grok warns and continues. Network restrictions cover only child processes. The per-command backend is macOS-only.
- **Prompt obfuscation** adds build machinery for no security benefit, as its own comment acknowledges.
- **Auto mode accepts every file edit** by product decision, so it relies on rewind and git to undo bad edits rather than on blocking them.

**Lessons for building a harness**
1. Put a protocol boundary (ACP-like) between UI and agent from day one, even in-process. Editors, headless runs and multi-client modes then come almost for free.
2. Make the model-facing tool surface data: ids, renames, params, and kinds feeding templated prompts. This makes dialect experiments and model-specific tool formats cheap. Validate incompatible mixes at load time.
3. Consider anchor-based edits (hashline) as an alternative to string replacement. Validate every anchor against a snapshot, apply bottom-up, and return fresh anchors.
4. Treat loop pathologies as harness responsibilities: canonicalized repeat detection, length salvage, todo gates, stop hooks, and an explicit `max_turns`. Record telemetry for each so thresholds can be tuned.
5. For auto-approval, layer a cheap deterministic pass in front of an LLM judge. Keep trusted facts as enum tokens, label project instructions as untrusted, fail closed on parse errors when risk was flagged, and escalate to the user after repeated denials.
6. Write down the persistence durability contract (what is lost on power failure) and route all writes through one actor.
7. Defer MCP tools behind a search/use pair (BM25) to keep the tool list and prompt cache stable as servers come and go.
8. Read other harnesses' config formats (CLAUDE.md, `.claude/`, Cursor rules, `.mcp.json`, hook names). Users switch harnesses, and compatibility makes switching cheap.
