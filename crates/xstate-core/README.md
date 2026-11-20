# xstate-core

Core engine of **xstate-statemachine** for Rust: an XState v5 / W3C SCXML compatible statechart
runtime. Load a machine exported from the [Stately editor](https://stately.ai/editor) as JSON,
supply the named guards, actions and services in Rust, and run it.

Status: 0.1.2. Parser, compiler, sans-I/O step engine, synchronous interpreter, Python-compatible
snapshots and structure hash, pure API, builder, exporters (JSON, Mermaid, PlantUML), zero-cost
native enum state machines (`fsm::Fsm`, `fsm::FsmInstance`), AST code generation (`codegen`), and
bidirectional XState export (`fsm::ExportXState`).

The engine matches XState v5 on every conformance case. No I/O, no async runtime, no required features.
See `docs/adr/` in the repository for the design.

### Dynamic XState v5 Statecharts (69.8 ns)

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

### Zero-Cost Native Enum State Machines (< 5 ns; 1.15–1.76 ns p50)

For ultra-low latency domains (HFT, robotics, embedded, game loops) that require compile-time
exhaustive pattern matching and zero heap allocations:

```rust
use xstate_core::fsm::{Fsm, FsmInstance, InvalidTransition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ToggleState { Inactive, Active }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ToggleEvent { Toggle }

pub struct ToggleMachine;

impl Fsm for ToggleMachine {
    type State = ToggleState;
    type Event = ToggleEvent;

    fn initial() -> Self::State { ToggleState::Inactive }

    fn step(state: Self::State, event: Self::Event) -> Result<Self::State, InvalidTransition<Self::State, Self::Event>> {
        match (state, event) {
            (ToggleState::Inactive, ToggleEvent::Toggle) => Ok(ToggleState::Active),
            (ToggleState::Active, ToggleEvent::Toggle) => Ok(ToggleState::Inactive),
        }
    }

    fn all_states() -> &'static [Self::State] { &[ToggleState::Inactive, ToggleState::Active] }
    fn all_events() -> &'static [Self::Event] { &[ToggleEvent::Toggle] }
}

let mut inst = FsmInstance::<ToggleMachine>::new();
assert_eq!(inst.send(ToggleEvent::Toggle), Ok(ToggleState::Active));
```

## Features

None. Most programs depend on the [`xstate-statemachine`](../xstate-statemachine) facade instead.

## Python counterpart

The `xstate_statemachine` package itself: [`interpreter`, `models`, `actions`, `graph`,
`persistence/snapshot`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine).
