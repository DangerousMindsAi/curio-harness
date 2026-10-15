# curio-harness

An agent harness, in early design. Groundwork research on existing harnesses lives under `docs/`: an [architecture survey](docs/harness-survey/README.md) and a close reading of each [agent loop](docs/agent-loops/README.md).

The first design proposal is the [agent loop proposal](docs/agent-loop-proposal/README.md): a plugin-first loop with checkpoint, resume and graceful quiesce for many isolated agents in one process.
