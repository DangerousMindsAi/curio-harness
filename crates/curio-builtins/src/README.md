# curio-builtins/src

One file per runner or group of related runners. Each file's header describes its runner's code (the YAML it accepts) with an example.

| File | Runners | Notes |
|---|---|---|
| [lib.rs](lib.rs) | | `registry()`, which registers every runner below. Start here to see the full list. |
| [driver.rs](driver.rs) | `builtin.driver.react` | The agent loop. The longest file and the one to read to understand a turn. |
| [model_openai.rs](model_openai.rs) | `builtin.model.openai-compat` | The HTTP request, SSE stream parsing, tool call assembly and error classification. |
| [model_scripted.rs](model_scripted.rs) | `builtin.model.scripted` | Fixed replies chosen by how far the conversation has got. |
| [queue_file.rs](queue_file.rs) | `builtin.queue.file` | The file-backed message queue. |
| [logstore_file.rs](logstore_file.rs) | `builtin.logstore.file` | The file-backed session log with epoch fencing. |
| [provisioner_yaml.rs](provisioner_yaml.rs) | `builtin.provisioner.yaml-dir` | Agent configs from YAML files. |
| [secrets_inline.rs](secrets_inline.rs) | `builtin.secrets.inline` | Secrets from the host file. |
| [policies.rs](policies.rs) | `builtin.instructions`, `builtin.retry`, `builtin.approval` | Plugins that only listen to hooks. |
| [tools.rs](tools.rs) | `builtin.tool.clock`, `.shell`, `.read_file`, `.write_file` | Tools confined to the plugin's `grants.fs` workspace. |
| [fsutil.rs](fsutil.rs) | | Helpers shared by the file-backed runners: safe path components, atomic writes, workspace path checks, truncation. |
