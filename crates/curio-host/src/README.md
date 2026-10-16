# curio-host/src

| File | What it holds |
|---|---|
| [main.rs](main.rs) | Command-line parsing, reading the host file, starting the runtime, choosing between create, thaw and recover, and saving images. |
| [repl.rs](repl.rs) | The terminal conversation: reading your input, printing the agent's live output, approvals, and draining the agent when you leave. |

`main.rs` calls `repl::run` once the agent is running, and `repl.rs` calls back into `main.rs` only to save the image.
