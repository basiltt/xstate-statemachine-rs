# Getting started

This page takes you from `cargo add` to a running machine in five minutes, then points at the
rest of the guide. Every Rust snippet on it is compiled by the test suite
(`crates/xstate-statemachine/tests/guide.rs`).

## Install

```toml
[dependencies]
xstate-statemachine = "0.1"
serde_json = "1"
```

The facade crate always contains the engine. Everything else is a feature flag that mirrors an
extra of the Python library (`pip install "xstate-statemachine[redis]"` is
`features = ["redis"]`):

```toml
xstate-statemachine = { version = "0.1", features = ["tokio", "sqlite", "inspect"] }
```

The full list is in [features.md](features.md). Rust 1.88 or newer is required (1.94 with the `postgres` feature, because of `sqlx` 0.9).

## 1. Run a chart

A machine is data (XState v5 JSON); the guards, actions, services and delays it names are Rust
functions you register by name. `MachineLogic::dynamic()` is the registry for the JSON
(`serde_json::Value`) context that machines loaded from JSON use.

```rust
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{
  "id": "door", "initial": "closed", "context": {"opens": 0, "locked": false},
  "states": {
    "closed": {"on": {"OPEN": {"target": "opened", "guard": "isUnlocked", "actions": "countOpen"}}},
    "opened": {"on": {"CLOSE": "closed"}}
  }
}"#)?;

let logic = MachineLogic::dynamic()
    .guard("isUnlocked", |ctx, _event, _params| Ok(ctx["locked"] != true))
    .action("countOpen", |scope| {
        let n = scope.context["opens"].as_u64().unwrap_or(0) + 1;
        scope.context["opens"] = n.into();
        Ok(())
    });

let mut door = SyncInterpreter::new(machine, logic);
door.start()?;
door.send("OPEN")?;
assert_eq!(door.state_ids(), ["door.opened"]);
door.send("BANANA")?; // declared nowhere: ignored (set "strict": true to make it an error)
# Ok::<(), Box<dyn std::error::Error>>(())
```

Things worth knowing:

- State ids are `<machine id>.<path>`, as in the Python library.
- A name the chart uses but the registry lacks is reported by `start()` as
  `ImplementationMissing`, naming the guard / action / service / delay, before any event runs.
- `send` accepts a `&str`, an `Event` with a payload, or a typed event (see below) and returns a
  `Receipt` (`changed`, `unhandled`, the user actions that ran, the status and `state_ids()`).
- The engine performs no I/O. `SyncInterpreter` runs timers on a clock you control
  (`SimulatedClock` in tests, `RealClock` otherwise), which makes `after` transitions testable
  without sleeping: `interp.advance(&clock, 31_000.0)`.

## 2. Run it asynchronously (feature `tokio`)

`spawn` runs a machine as a tokio task that owns its state and returns a cheap, cloneable handle.
Services can be `async`.

```rust
use xstate_statemachine::prelude::*;
use xstate_statemachine::tokio::Options;

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let machine = Machine::from_json(r#"{"id":"fetch","initial":"idle","states":{
    "idle":{"on":{"FETCH":"loading"}},
    "loading":{"invoke":{"src":"load","onDone":"done","onError":"failed"}},
    "done":{},"failed":{}}}"#)?;
let logic = MachineLogic::dynamic()
    .async_service("load", |_ctx, _input| Box::pin(async { Ok(serde_json::json!("ok")) }));

let actor = spawn(machine, logic, Options::default())?;
let mut view = actor.subscribe();
actor.send("FETCH").await?;
view.wait_for(|v| v.state_ids == ["fetch.done"]).await?;
actor.stop().await;
# Ok(()) }
```

`after` timers run on `tokio::time`, so `tokio::time::pause()` works in tests.

## 3. Typed events and states (feature `macros`)

When you would rather have the compiler check the chart, `statechart!` builds a `Machine` from a
Rust literal and the derives give you enums for events and states:

```rust
use xstate_statemachine::prelude::*;

#[derive(TypedEvent)]
#[event(rename_all = "SCREAMING_SNAKE_CASE")]
enum DoorEvent { Open, Close }

#[derive(Debug, Clone, Copy, PartialEq, TypedState)]
#[state(machine = "door")]
enum Door { Closed, Opened }

let machine = statechart! {
    id: "door",
    initial: closed,
    states: {
        closed { on { OPEN => opened } }
        opened { on { CLOSE => closed } }
    }
}?;
let mut door = SyncInterpreter::new(machine, MachineLogic::dynamic());
door.start()?;
door.send(DoorEvent::Open)?;
assert!(Door::Opened.is_active(&door));
# Ok::<(), xstate_statemachine::core::Error>(())
```

Unresolvable targets, duplicate states and an `initial` that names no child state are compile errors.

## 4. Use the command line

```bash
cargo install xstate-statemachine-cli   # installs the `xsm` binary
xsm validate door.json            # build it with the real engine, report every finding
xsm inspect door.json             # states, transitions and the logic names you still have to write
xsm simulate door.json -e OPEN,CLOSE
xsm diagram door.json -f mermaid
```

## Where next

- Design in the Stately editor and run the export: [stately-editor.md](stately-editor.md)
- Save and resume instances (SQLite, Redis, PostgreSQL): [persistence.md](persistence.md)
- Consume and publish events on a broker: [eda.md](eda.md)
- Coming from the Python library: [migrating-from-python.md](migrating-from-python.md)
- Every feature flag: [features.md](features.md)
