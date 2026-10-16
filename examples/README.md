# examples

Configuration for running agents locally with `curio`.

| File | What it is |
|---|---|
| [host.example.yaml](host.example.yaml) | The host's startup file. Copy it to `host.yaml` at the repository root and add your DeepInfra key; `host.yaml` is gitignored. It points the provisioner at [agents/](agents). |
| [agents/](agents) | Agent templates, one YAML file per agent. `curio chat <name>` creates an agent from `agents/<name>.yaml`. |

Keys never go in agent files: an agent's configuration is written into its session log and every saved image, so agents name a secret and the host's secret store holds its value.
