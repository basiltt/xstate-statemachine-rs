# xstate-core

Core engine of **xstate-statemachine** for Rust: an XState v5 / W3C SCXML compatible statechart
runtime. Load a machine exported from the [Stately editor](https://stately.ai/editor) as JSON,
supply the named guards, actions and services in Rust, and run it.

Status: 0.1.0. Parser, compiler, sans-I/O step engine, synchronous interpreter, Python-compatible
snapshots and structure hash, pure API, builder, exporters (JSON, Mermaid, PlantUML) and graph
utilities. The engine matches XState v5 on every conformance case. No I/O, no async runtime, no
required features. See `docs/adr/` in the repository for the design.

```rust
use xstate_core::{Machine, MachineLogic, SyncInterpreter};

let machine = Machine::from_json(r#"{
  "id": "toggle", "initial": "inactive",
  "states": { "inactive": { "on": { "TOGGLE": "active" } },
              "active":   { "on": { "TOGGLE": "inactive" } } }
}"#)?;
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.start()?;
interp.send("TOGGLE")?;
assert_eq!(interp.state_ids(), ["toggle.active"]);
# Ok::<(), xstate_core::Error>(())
```

## Features

None. Most programs depend on the [`xstate-statemachine`](../xstate-statemachine) facade instead.

## Python counterpart

The `xstate_statemachine` package itself: [`interpreter`, `models`, `actions`, `graph`,
`persistence/snapshot`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine).
