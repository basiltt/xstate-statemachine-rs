# xsm (package `xstate-statemachine-cli`)

The command-line companion of xstate-statemachine: validate, inspect, diagram, document,
simulate and analyse XState machines, serve the Stately Inspector locally, and inspect snapshot
stores, from the terminal. Output is plain text when piped; `--json` is available on the
analysis commands. Exit codes: `0` success, `1` findings or a failed run, `2` usage error.

```bash
cargo install xstate-statemachine-cli   # installs the `xsm` binary
xsm validate machine.json                 # build with the real engine, report every finding
xsm inspect machine.json                  # state tree, transitions, logic, policies
xsm diagram machine.json -f mermaid       # mermaid | plantuml | json
xsm simulate machine.json -e GO,+500,STOP --record run.jsonl
xsm paths machine.json --simple           # a path to every configuration
xsm inspect machine.json --live           # Stately Inspector over SSE, driven from stdin
xsm replay run.jsonl --live --speed 1     # replay a recording into the Inspector
xsm snapshots --store sqlite:///app.db machine.json --stale
```

| Command | Python `xsm` counterpart |
|---|---|
| `validate`, `inspect`, `diagram`, `docs`, `simulate`, `paths`, `snapshots`, `info`, `replay` | same names |
| not yet: `coverage`, `asyncapi`, `dlq`, `generate-template`, `new`, `plugins`, `update`, `setup` | see the root README parity matrix |

## Features

None. Binary crate; depends on `xstate-core`, `xstate-persistence` and `xstate-inspect`.

## Python counterpart

[`xstate_statemachine.cli`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/cli)
(`python -m xstate_statemachine`, also installed as `xsm`).
