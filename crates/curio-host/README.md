# curio-host

The `curio` binary: a local host for running and talking to one agent at a time from the terminal. It is the smallest real host for the harness, and the place to look for how a host is meant to use `curio-core`.

A host has three jobs that the harness leaves to it: reading the startup configuration, deciding whether an agent should be created, thawed or recovered, and keeping images somewhere after a drain. This one does all three with local files:

- The startup file is `host.yaml` (see [examples/host.example.yaml](../../examples/host.example.yaml)). Paths in it, and in agent files, are relative to its directory.
- On `curio chat <template>`, a saved image in `images_dir` means thaw, a session log without an image means recover, and neither means create.
- On `/quit` or `/pause` it drains the agent and writes the image to `images_dir/<agent>.json`; the next start thaws it and deletes the file.

Usage is in the [root README](../../README.md#running-an-agent-locally); [src/README.md](src/README.md) describes the two source files.
