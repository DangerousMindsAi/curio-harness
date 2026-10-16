# curio-builtins/tests

End-to-end tests of the agent loop running with the built-in plugins. They live here, not in curio-core, because they need real runners.

[loop.rs](loop.rs) starts real runtimes in temporary directories, writes agent YAML files there, and talks to agents through their handles exactly as a host would. Most tests use the scripted model, so they need no network and run in well under a second. The suite covers a turn with a tool call, approval, quiesce and thaw on a second runtime, a drain at a step boundary, a crash in the middle of a tool and the recovery that follows, fencing of an old log owner, refusal of an out-of-date image, two isolated agents in one process, secret grants, and the OpenAI-compatible model against a local fake HTTP server that first returns 503 to exercise the retry plugin.

Run them with `cargo test -p curio-builtins --test loop`.
