# Sync interpreter

The synchronous interpreter lives in `xstate-core` (re-exported by the facade). It drives the
sans-I/O step engine inline: no runtime, no tasks, no I/O. This is the interpreter for request /
response services, tests and the CLI.

```rust
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"
  {"id": "door", "initial": "closed",
   "states": {"closed": {"on": {"OPEN": "opened"}}, "opened": {"on": {"CLOSE": "closed"}}}}
"#)?;
let mut door = SyncInterpreter::new(machine, MachineLogic::dynamic());
door.start()?;
door.send("OPEN")?;
assert_eq!(door.state_ids(), ["door.opened"]);
# Ok::<(), xstate_statemachine::core::Error>(())
```

## The API

- `SyncInterpreter::new(machine, logic)` uses a `RealClock`; `with_clock(machine, logic, clock)`
  takes a `SimulatedClock` so tests can move time with `advance(&clock, ms)` instead of sleeping.
- `start()` runs the initial macrostep and reports missing implementations. `send(event)` takes a
  `&str`, an `Event` with a payload, or a typed event, and returns a `Receipt` (`changed`,
  `unhandled`, the actions that ran, `status()` and `state_ids()`).
- `state_ids()` lists the active leaf state ids (`<machine id>.<path>`); `context()` returns the
  current context; `status()` is `running`, `done`, `error` or `stopped`.
- `snapshot()` / `snapshot_json()` capture the instance; `SyncInterpreter::from_snapshot(..)`
  restores one (see [snapshots](../concepts/snapshots.md)).
- `observe(Arc<dyn Observer>)` attaches observers (metrics, inspector, transition log).

The core crate also exposes the bare step function and the pure API
(`initial_transition`, `transition`) for programs that want the algorithm without an interpreter,
and graph utilities (`stub_logic`, `reachable_states`, `shortest_paths`, `simple_paths`) for
analysis. The async equivalent is [`spawn` on tokio](tokio.md); the two share one algorithm
(ADR-0004).
