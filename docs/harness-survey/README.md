# Harness Survey

Architecture notes on three open coding-agent harnesses, written as groundwork for curio-harness.

- [comparison.md](comparison.md): side-by-side comparison, shared patterns, what is unique about each, and implications for curio-harness. Start here.
- [opencode.md](opencode.md): opencode V2 (TypeScript, Effect, Bun), a client/server agent with an event-sourced core.
- [deepseek-harness.md](deepseek-harness.md): DeepSeek Harness (TypeScript, Cordis), an all-plugin harness with a strictly event-sourced session log.
- [grok-build.md](grok-build.md): Grok Build (Rust), xAI's terminal agent with tool dialects, hashline edits and an LLM-judged auto mode.

Each per-harness report follows the same outline (identity, layout, process topology, agent loop, providers, tools, context, sessions, safety, extensibility, clients, configuration, testing, distinctive features, lessons) and cites `path:line` in the surveyed repository. The snapshots are:

| Repository | Commit | Date |
|---|---|---|
| [DangerousMindsAi/opencode](https://github.com/DangerousMindsAi/opencode) | `8ffbcc2` | 2026-10-14 |
| [DangerousMindsAi/deepseek-harness](https://github.com/DangerousMindsAi/deepseek-harness) | `d743267` | 2026-10-09 |
| [DangerousMindsAi/grok-build](https://github.com/DangerousMindsAi/grok-build) | `2bdd1d6` | 2026-09-29 |

For a closer look at one subsystem, see [Agent Loops](../agent-loops/README.md): pseudo-code, configuration and plugin hooks for each harness's core loop.
