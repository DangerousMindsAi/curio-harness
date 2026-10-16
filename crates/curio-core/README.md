# curio-core

The harness core: everything an agent needs except the plugins themselves. The design it implements is the [agent loop proposal](../../docs/agent-loop-proposal/README.md); this README explains how the code is organized around it.

## Concepts

**Runners and plugins.** A *runner* is Rust code compiled into the host and registered under a name such as `builtin.tool.shell` (the [`Runner`](src/plugin.rs) trait). An agent's configuration lists *plugins*, each naming a runner and giving it *code* to load: for built-in runners the code is a YAML table. `Runner::load` returns an immutable plugin instance plus its initial *state slot*. A plugin provides services (driver, model, tool, queue) through `as_*` methods and listens to hooks through `on_hook`.

**The agent context.** [`AgentCtx`](src/agent.rs) holds every piece of one agent's mutable state: its config, plugin table, state slots, journal, queue consumer, phase and cancellation token. One tokio task owns it for the agent's whole life. Callers outside reach the agent only through an `AgentHandle`, which sends commands over a channel and publishes messages to the agent's queue.

**The journal.** The [`Journal`](src/log.rs) pairs the agent's session log with a [`View`](src/event.rs) folded from it: the model-visible history, the inbox of waiting messages, counters and open work. Appending an event writes it to the log and folds it into the view in one call, and recovery folds the same events in the same way, so a recovered agent sees exactly what the live one saw. The log is forced to durable storage at fixed save points: before each model request, before any tool body runs, after pulling messages and before acknowledging them, at the end of each turn, and at quiesce.

**Hooks.** The driver calls [`dispatch`](src/hook.rs) at defined points in a turn. Each subscribed plugin gets a `HookCx` (its own state slot mutably, the rest of the agent through the view and the log) and the hook's payload, which it may change in place. Listeners run in order and continue by default; a plugin stops the chain explicitly.

**Three ways to start.** The [`Runtime`](src/runtime.rs) brings an agent to life by one of three paths, all of which load plugins the same way:

- `create` asks the provisioner for a config, writes the log header and starts the agent.
- `thaw` restores a quiesced agent from its [`AgentImage`](src/image.rs): config, every slot, the view and the log position. It reads nothing from the log.
- `recover` rebuilds an agent that stopped without quiescing by replaying its log, then closes any interrupted turn with honest synthetic tool results.

**Quiesce.** A drain command asks the agent to stop at a step or turn boundary. `AgentCtx::settle_drain` then snapshots plugin state, logs `Quiesced`, serializes the context into an image, releases the log, detaches from the queue and hands the image to whoever asked.

## Where to start reading

Read [src/README.md](src/README.md) for the file-by-file map. To follow one message through the system, the order is: `AgentHandle::send` publishes it, `AgentCtx::pull_messages` logs it, the driver's turn admits it into the history (`View::fold` on `TurnStarted`), the model request is built from `History::to_request_messages`, and the reply comes back as an `AssistantReply` event.

The default loop itself is not here; it is the driver plugin in [curio-builtins](../curio-builtins/src/driver.rs).
