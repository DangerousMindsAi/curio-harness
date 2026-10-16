# curio-core/src

One module per concept. The table is in reading order: each file mostly depends on the ones above it.

| File | What it holds |
|---|---|
| [lib.rs](lib.rs) | The crate root: module list, re-exports of the public types, and `now_ms`. |
| [ids.rs](ids.rs) | Newtype ids (`AgentId`, `PluginId`, `TurnId`, `QueueMsgId`, ...), so different kinds of id cannot be mixed up. |
| [error.rs](error.rs) | The shared `Error` type: configuration and load failures, log fencing and stale images, queue and secret errors. |
| [config.rs](config.rs) | `AgentConfig` (an agent's plugins, grants and crash policy) and `RuntimeConfig` (the host's startup file). Plain data read from YAML. |
| [message.rs](message.rs) | Conversation and model types: chat messages, tool calls and specs, model requests and replies, model errors. |
| [event.rs](event.rs) | Session log `Event`s and the `View` folded from them: history, inbox, counters and open work. The fold is the single place where events change derived state. |
| [log.rs](log.rs) | The `LogStore` and `SessionLog` traits that log store plugins implement, and the `Journal` that keeps a log and its view in step. |
| [queue.rs](queue.rs) | The `MessageQueue` and `QueueConsumer` traits that queue plugins implement. |
| [hook.rs](hook.rs) | Hook names, payloads (`HookEvent`), decisions, the `HookCx` a listener receives, and `dispatch`. |
| [plugin.rs](plugin.rs) | The `Runner` and `Plugin` traits, state slots, the service traits (driver, model, tool, secrets, provisioner), and the `PluginTable` an agent's plugins are indexed in. |
| [image.rs](image.rs) | `AgentImage`, the serialized context of a quiesced agent. |
| [agent.rs](agent.rs) | `AgentCtx`, commands, observations, drains, and `AgentHandle`. Also the helpers every driver uses: waiting while handling commands, pulling messages, and quiescing. |
| [runtime.rs](runtime.rs) | `RunnerRegistry`, `Runtime` with `create`, `thaw` and `recover`, and crash repair. |

Unit tests for the fold live at the bottom of `event.rs`. End-to-end tests of the whole loop are in [curio-builtins/tests](../../curio-builtins/tests), because they need real plugins.
