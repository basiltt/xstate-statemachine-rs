# xstate-macros

Procedural macros for `xstate-core`:

* `#[derive(TypedEvent)]`: an enum as the typed event vocabulary of a machine (rename rules,
  payload mapping, `From` / `TryFrom<Event>`, `declared_events()`, `check` against a `Machine`);
* `#[derive(TypedState)]`: an enum naming a machine's leaf states (`is_active`, `check`);
* `statechart!`: a Rust-literal chart DSL covering every XState construct, validated at compile
  time (duplicates, initial children, target resolution, tree shape) and building a `Machine`.

Status: 0.1.0, with `trybuild` UI tests for the diagnostics. The derives need `xstate-core` (and
`serde` for payload types) in the calling crate; `statechart!` additionally needs `serde_json`.

```rust
use xstate_core::{MachineLogic, SyncInterpreter};
use xstate_macros::{TypedEvent, TypedState, statechart};

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
# Ok::<(), xstate_core::Error>(())
```

## Features

None (a proc-macro crate).

## Python counterpart

The Pythonic DSL ([`xstate_statemachine.pythonic`](https://github.com/basiltt/xstate-statemachine/blob/main/src/xstate_statemachine/pythonic.py)):
class-based `StateMachine` / `State`, the builder and the functional form. Rust gets typed enums
and a compile-time-checked macro instead of metaclass scanning.
