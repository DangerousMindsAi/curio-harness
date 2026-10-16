# crates

The Rust workspace. Three crates, each depending only on the ones above it in this table:

| Crate | Role |
|---|---|
| [curio-core](curio-core) | The harness itself: the per-agent context, the runner and plugin interfaces, hooks, the event-sourced session log, agent images, and the runtime that creates, thaws and recovers agents. It contains no plugins and makes no network or file calls of its own. |
| [curio-builtins](curio-builtins) | The built-in runners: the default driver (the agent loop), model providers, a file-backed message queue and log store, the YAML provisioner, a secret store, policy plugins and tools. Everything an agent does is one of these plugins. |
| [curio-host](curio-host) | The `curio` binary: a local host that reads YAML configuration, starts a runtime with the built-in runners and lets you talk to one agent in the terminal. |

## How the pieces fit together

The split follows the "everything is a plugin" rule from the [proposal](../docs/agent-loop-proposal/README.md). `curio-core` defines what a plugin is and how plugins are called, but never decides what an agent does: even the loop is a plugin, the driver in `curio-builtins`. A host such as `curio-host` puts the two together by registering runners and starting a runtime.

```text
curio-host                 reads host.yaml, owns images, runs the terminal
   │  Runtime::start(config, curio_builtins::registry())
   ▼
curio-core::Runtime        runner registry + log store, provisioner, secret store
   │  create / thaw / recover
   ▼
AgentCtx (one task)        all of one agent's state; plugins loaded by runners
   │  driver.run(&mut cx)
   ▼
curio-builtins driver      turns and steps; dispatches hooks to the other plugins
   ├── model plugin        streams a reply for each request the driver builds
   ├── tool plugins        run one call each, read-only access to the agent
   ├── policy plugins      instructions, retry, approval: hook listeners
   └── queue plugin        where the agent's messages wait until it pulls them
```

Every agent is a separate task owning its own `AgentCtx`, so many agents with different configurations share one process without sharing state. Each crate's README goes one level deeper, and each source file starts with a description of what it holds.

## Working on the code

From the repository root, `cargo test` runs every unit and end-to-end test, `cargo clippy --all-targets` lints, and `cargo doc --open --document-private-items` renders the API documentation, which is where the per-function detail lives.
