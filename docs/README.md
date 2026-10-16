# docs

Design documents and the research behind them. The code under [crates](../crates) implements the agent loop proposal; the other documents are background.

| Folder | What it holds |
|---|---|
| [agent-loop-proposal](agent-loop-proposal/README.md) | The design curio-harness implements: plugins as runner plus code, the agent context, hooks, checkpoint and resume, quiesce, the message queue, migration and provisioning. |
| [harness-survey](harness-survey/README.md) | An architecture survey of three existing harnesses (opencode, DeepSeek Harness, Grok Build) and a comparison. |
| [agent-loops](agent-loops/README.md) | A close reading of each surveyed harness's agent loop. |
| [crash-recovery](crash-recovery/README.md) | How each surveyed harness recovers from a crash, with notes for curio-harness. |
