# examples/agents

Agent templates for the `builtin.provisioner.yaml-dir` provisioner. Each file is one agent's full configuration: its id and tenant, what happens after a crash (`resume`), and its plugins, each with a `runner`, its `code` and its `grants`.

| File | Agent |
|---|---|
| [assistant.yaml](assistant.yaml) | A general assistant on DeepInfra with a workspace it can read, write and run shell commands in. Shell commands need your approval. |
| [offline.yaml](offline.yaml) | A scripted agent that needs no API key, for trying save, resume and crash recovery. |

To make your own agent, copy one of these and change the plugins. Each runner's accepted code is described at the top of its source file in [crates/curio-builtins/src](../../crates/curio-builtins/src). The configuration is pinned in the agent's log when it is created, so editing a template later changes new agents only.
