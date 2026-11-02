# xstate-statemachine

**Statecharts for Rust, compatible with [XState v5](https://stately.ai/docs/xstate) and the
[Stately editor](https://stately.ai/editor).** The facade crate: one dependency that re-exports
the engine and, behind feature flags that mirror the Python library's extras, the async runtime,
persistence, event-driven architecture, patterns, observability, the inspector, the typed DSL,
axum integration and agents.

Status: 0.1.0. The Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine); snapshots, store
layouts, envelopes and inspector recordings are byte-compatible with it.

```rust
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{
  "id": "toggle", "initial": "inactive",
  "states": { "inactive": { "on": { "TOGGLE": "active" } },
              "active":   { "on": { "TOGGLE": "inactive" } } }
}"#)?;
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.start()?;
interp.send("TOGGLE")?;
assert_eq!(interp.state_ids(), ["toggle.active"]);
# Ok::<(), xstate_statemachine::core::Error>(())
```

## Features

All off by default; the engine is always present. `tokio`, `persistence` (+ `sqlite`, `redis`,
`postgres`), `eda` (+ `redis-streams`, `nats`, `rabbitmq`, `sqs`), `patterns`, `observability`,
`inspect`, `macros`, `axum`, `agents`, `testing`, and `full` for all of them. Each feature adds a
module of the same name and the matching items of `prelude`. The table is in the crate docs and in
[`docs/guide/features.md`](../../docs/guide/features.md).

```toml
xstate-statemachine = { version = "0.1", features = ["tokio", "sqlite", "inspect"] }
```

## Python counterpart

The whole [`xstate_statemachine`](https://github.com/basiltt/xstate-statemachine) package and its
extras (`pip install "xstate-statemachine[redis]"` is `features = ["redis"]`).
