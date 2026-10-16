# curio-builtins

The built-in runners. Each one is registered under a `builtin.*` name by [`registry()`](src/lib.rs), and its code is a YAML table in the agent's or host's configuration. Together they are enough to run a useful agent locally; anything else (other model providers, real sandboxes, brokers, WASM or Python plugins) would be another runner alongside these.

## Per-agent runners

These appear in an agent's `plugins` list.

| Runner | Kind | What it does |
|---|---|---|
| `builtin.driver.react` | driver | The agent loop: turns, steps, retries, the tool pipeline and stop conditions. Every agent needs exactly one driver. |
| `builtin.model.openai-compat` | model | Calls any OpenAI-compatible chat completions API, such as DeepInfra's, and streams the reply. |
| `builtin.model.scripted` | model | Plays back fixed replies, for tests and for trying the host without an API key. |
| `builtin.queue.file` | message queue | Keeps each agent's inbound messages in files, so they survive restarts. Every agent needs exactly one queue. |
| `builtin.instructions` | hook listener | Adds a system prompt when the agent is first loaded. |
| `builtin.retry` | hook listener | Retries transient model errors with exponential backoff. |
| `builtin.approval` | hook listener | Asks the user before, or refuses, calls to listed tools. |
| `builtin.tool.clock`, `builtin.tool.shell`, `builtin.tool.read_file`, `builtin.tool.write_file` | tools | The current time, a shell command, and reading and writing files in the agent's workspace. |

## Runtime-level runners

These appear once in the host's startup file and serve every agent in the process.

| Runner | What it does |
|---|---|
| `builtin.logstore.file` | Session logs as JSON Lines files, with an epoch file per agent for fencing. |
| `builtin.provisioner.yaml-dir` | Creates agents from `<template>.yaml` files in a directory. |
| `builtin.secrets.inline` | Secrets listed in the host file by `<tenant>/<name>`, as literal values or environment variable names. |

## How a turn uses them

When a message arrives, the driver pulls it from the queue plugin and starts a turn. Each step dispatches `pre_step` and `request` hooks, asks the model plugin for a reply (the retry plugin decides what happens if that fails), and then runs the tools the reply asked for: `tool_pre` lets the approval plugin allow, deny or ask, the tool plugins run their bodies, and `tool_post` sees each result before it is committed. Throughout, every input the model sees goes through the session log kept by the log store plugin. [src/driver.rs](src/driver.rs) describes this in detail.

## Files

See [src/README.md](src/README.md) for the file-by-file map and [tests/README.md](tests/README.md) for the end-to-end tests.
