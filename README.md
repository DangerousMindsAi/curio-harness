# curio-harness

An agent harness in Rust where everything is a plugin, built for many isolated agents in one process with checkpoint and resume.

The design is the [agent loop proposal](docs/agent-loop-proposal/README.md). Groundwork research on other harnesses lives under `docs/`: an [architecture survey](docs/harness-survey/README.md) and a close reading of each [agent loop](docs/agent-loops/README.md).

## Running an agent locally

You need Rust 1.88 or newer and, for a real model, a [DeepInfra](https://deepinfra.com) API key.

```sh
cp examples/host.example.yaml host.yaml   # host.yaml is gitignored
# edit host.yaml and replace YOUR_DEEPINFRA_API_KEY (or point it at an environment variable)
cargo run -p curio-host -- chat assistant
```

`chat assistant` creates the agent from [examples/agents/assistant.yaml](examples/agents/assistant.yaml) the first time. Type a message and press Enter. While the agent works, anything you type is added to its next step.

| Command | What it does |
|---|---|
| `/quit` or Ctrl-D | Lets the current turn finish, saves the agent to `data/images/<id>.json` and exits. |
| `/pause` | Stops at the next step boundary, saves the agent mid-turn and exits. The turn continues on the next start. |
| `/cancel` or Ctrl-C | Cancels the current turn. |
| `/kill` | Exits at once without saving, as a crash would. The next start recovers the agent from its log. |

Running the same command again resumes the agent: from its saved image if there is one, otherwise from its log. Messages sent while it is stopped wait in its queue. `--id other` runs a second agent from the same template.

To try saving and resuming without an API key, use the scripted agent: `cargo run -p curio-host -- chat offline`.

The `shell` tool is not sandboxed; it only runs in the agent's workspace directory. The example agent therefore asks for your approval before every command.

## Configuration

Everything is YAML. The host file ([example](examples/host.example.yaml)) is the process's startup file: it names the runners agents may use and the three runtime-level plugins (log store, provisioner and secret store). Each agent file ([example](examples/agents/assistant.yaml)) lists the agent's plugins, each with a `runner`, its `code` (for built-in runners, a configuration table) and its `grants`.

API keys live only in the host file's secret store, as `<tenant>/<name>`. An agent's model plugin names the secret and must also list it in `grants.secrets`. Secret values are resolved per request and never written to logs or images.

## Layout

| Crate | Contents |
|---|---|
| [curio-core](crates/curio-core) | The agent context, plugin and runner interfaces, hooks, session log, agent image and the runtime (create, thaw, recover). |
| [curio-builtins](crates/curio-builtins) | Built-in runners: the default driver (the loop), the OpenAI-compatible model used for DeepInfra, a scripted model, a file-backed queue and log store, the YAML provisioner, inline secrets, instructions, retry, approval and the `clock`, `shell`, `read_file` and `write_file` tools. |
| [curio-host](crates/curio-host) | The `curio` binary: a local host that runs agents from YAML and talks to them in the terminal. |

`cargo test` runs the unit tests and the end-to-end loop tests in `crates/curio-builtins/tests/loop.rs`, which cover tools, approval, quiesce and thaw on a fresh runtime, a drain at a step boundary, crash recovery mid-tool, log fencing, stale images, two isolated agents, and the DeepInfra plugin against a local fake server.

## Status against the proposal

Implemented as proposed: plugins as runner plus code, immutable plugin instances with state slots, the agent context owned by one task, the default driver with its hooks, write-ahead save points, quiesce at a turn or step boundary into an image, thaw, crash recovery with honest repair and optional continuation, the pull-based queue plugin, epoch fencing, and the provisioner and log store as runtime-level plugins.

Differences and gaps:

- Code is inline YAML only. The `wasm`, `process` and `mcp` runners, file and package code, and the WIT hook definition come later.
- Secrets come from a third runtime-level plugin, `secret_store`, so that keys stay out of agent configs, logs and images.
- The `tool_execute` hook is not implemented; the driver applies `tool_timeout_secs` to each tool body instead.
- `ErrorDecision::Retry` carries the delay, and the driver waits it out so a cancel or drain still gets through.
- Not yet implemented: the `Reconfigure` command, parking an agent that waits on an approval, periodic snapshots, and the scheduler with tenant quotas. A drain that reaches an agent waiting on an approval waits until its deadline, then cancels the turn.
