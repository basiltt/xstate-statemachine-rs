# xsm (package `xstate-statemachine-cli`)

The command-line companion of xstate-statemachine: validate, inspect, diagram, document,
simulate and analyse XState machines, serve the Stately Inspector locally, and inspect snapshot
stores, from the terminal. Output is plain text when piped; `--json` is available on the
analysis commands. Exit codes: `0` success, `1` findings or a failed run, `2` usage error.

```bash
cargo install xstate-statemachine-cli   # installs both `xsm` and `xsm-rs` binaries
xsm validate machine.json                 # build with the real engine, report every finding
xsm inspect machine.json                  # state tree, transitions, logic, policies
xsm diagram machine.json -f mermaid       # mermaid | plantuml | json
xsm simulate machine.json -e GO,+500,STOP --record run.jsonl
xsm paths machine.json --simple           # every acyclic path (default: a shortest path to each configuration)
xsm inspect machine.json --live           # Stately Inspector over SSE, driven from stdin
xsm replay run.jsonl --live --speed 1     # replay a recording into the Inspector
xsm snapshots --store sqlite:///app.db machine.json --stale      # records labelled with another machine version
xsm snapshots --store sqlite:///app.db machine.json --stale --stale-by hash   # structure hash differs

# In polyglot environments with Python installed, `xsm-rs` is 100% unambiguous:
xsm-rs validate machine.json
```

| Command | Python `xsm` counterpart |
|---|---|
| `validate`, `inspect`, `diagram`, `docs`, `simulate`, `paths`, `snapshots`, `info`, `replay` | same names |
| not yet: `coverage`, `asyncapi`, `dlq`, `generate-template`, `list-templates`, `new`, `plugins`, `update`, `setup` | see the root README parity matrix |

`snapshots --stale` follows the Python CLI: a record is stale when its `machine_version` label
differs from the machine JSON's `version` (an unversioned machine or an unlabelled record is never
stale). `--stale-by version|hash|either` (default `version`) picks the rule, and the table and
`--json` always report a `hash_mismatch` flag, the structure difference restore would refuse.
Only `sqlite:///` and `file:///` stores are readable; there is no `memory://`. `validate` treats
unknown keys as errors unless `--lenient`.

## Coexistence with Python `xsm` & PATH Precedence

Both the Python library ([`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine)) and this Rust package (`xstate-statemachine-cli`) provide an executable named `xsm`. If you have both installed on your system, the command that runs when you type `xsm` is decided entirely by your shell's **`PATH` precedence**:

### How Shell PATH Resolution Works
1. **Active Python Virtualenv:**
   When you activate a Python virtual environment (`source .venv/bin/activate` or `.venv\Scripts\Activate.ps1`), the virtualenv's `bin` directory is **prepended** to the front of `$PATH`. While active, typing `xsm` will execute the Python CLI.
2. **Global Cargo Bin vs. Global Python:**
   Outside an active virtual environment, if `$HOME/.cargo/bin` appears before your Python script directory (e.g. `~/.local/bin` or Windows `%USERPROFILE%\AppData\Local\Programs\Python\Scripts`) in your `$PATH`, typing `xsm` will execute the native Rust CLI.

### Guaranteed Disambiguation
To eliminate any ambiguity in polyglot development environments and CI pipelines:

1. **Native Rust Binary (`xsm-rs`):**
   `cargo install xstate-statemachine-cli` installs **both** `xsm` and `xsm-rs`. The `xsm-rs` command is a dedicated alias guaranteed to invoke the native Rust engine regardless of Python virtual environment states or PATH order:
   ```bash
   xsm-rs validate machine.json
   xsm-rs inspect machine.json --live
   ```

2. **Python Entry Point (`python -m`):**
   The Python implementation can always be invoked unambiguously via Python's module flag:
   ```bash
   python -m xstate_statemachine validate machine.json
   ```

3. **Verify Which CLI is Active:**
   You can verify which executable is currently answering `xsm` with your shell lookup and the built-in `info` command:
   ```bash
   # Which binary is on PATH:
   which xsm        # Linux / macOS
   where.exe xsm    # Windows PowerShell / CMD

   # Check engine metadata:
   xsm info
   # In Rust, output identifies crate as:
   #   "name": "xsm",
   #   "crates": { "xsm": "0.1.0", "xstate-core": "0.1.0", ... }
   ```

## Features

None. Binary crate; depends on `xstate-core`, `xstate-persistence` and `xstate-inspect`.

## Python counterpart

[`xstate_statemachine.cli`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/cli)
(`python -m xstate_statemachine`, also installed as `xsm`).
