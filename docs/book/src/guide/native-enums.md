# Zero-Cost Native Enum State Machines & Code Generation

`xstate-statemachine-rs` v0.1.2 introduces **native enum-based state machines** and automated **Rust code generation** from XState v5 JSON documents.

While the primary dynamic engine ([`SyncInterpreter`](../guide/sync-interpreter.md) and [`tokio`](../guide/tokio.md)) delivers industry-leading performance (**69.8 ns p50** via `send_fast`), high-frequency domains—such as High-Frequency Trading (HFT), robotics, and game engines—require transitions executing in **single-digit nanoseconds** (< 5 ns) with compile-time exhaustive match checks and zero heap allocations.

---

## Performance Target & Benchmark Profile

A pure enum transition compiles to an inlined direct CPU jump table or branch sequence (`#[repr(u8)]`):

| Mechanism | p50 Latency | Heap Allocations | Compile-Time Safety | Visual Editor Parity |
|---|---|---|---|---|
| **Native Enum FSM (`Fsm::step`)** | **1.15–1.63 ns** | **0 bytes** | Exhaustive compiler check | Full (via `ExportXState`) |
| **FSM Instance (`FsmInstance::send`)** | **0.91–1.76 ns** | **0 bytes** | Exhaustive compiler check | Full (via `ExportXState`) |
| **Dynamic `send_fast` (xstate-core)** | 69.8 ns | 0 bytes | Dynamic validation | Full (W3C SCXML / XState v5) |

---

## 1. Hand-Written Native Enum FSM

You can declare a zero-cost finite state machine directly in Rust without any JSON:

```rust
use xstate_core::fsm::{Fsm, FsmInstance, InvalidTransition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OrderState {
    Pending,
    Submitting,
    Open,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OrderEvent {
    Submit,
    SubmitAck,
    Fill,
    Cancel,
    Reject,
}

pub struct OrderMachine;

impl Fsm for OrderMachine {
    type State = OrderState;
    type Event = OrderEvent;

    #[inline]
    fn initial() -> Self::State {
        OrderState::Pending
    }

    #[inline]
    fn step(
        state: Self::State,
        event: Self::Event,
    ) -> Result<Self::State, InvalidTransition<Self::State, Self::Event>> {
        match (state, event) {
            (OrderState::Pending, OrderEvent::Submit) => Ok(OrderState::Submitting),
            (OrderState::Submitting, OrderEvent::SubmitAck) => Ok(OrderState::Open),
            (OrderState::Open, OrderEvent::Fill) => Ok(OrderState::Filled),
            (OrderState::Open, OrderEvent::Cancel) => Ok(OrderState::Cancelled),
            (OrderState::Submitting, OrderEvent::Reject) => Ok(OrderState::Rejected),
            _ => Err(InvalidTransition { state, event }),
        }
    }

    fn all_states() -> &'static [Self::State] {
        &[
            OrderState::Pending,
            OrderState::Submitting,
            OrderState::Open,
            OrderState::Filled,
            OrderState::Cancelled,
            OrderState::Rejected,
        ]
    }

    fn all_events() -> &'static [Self::Event] {
        &[
            OrderEvent::Submit,
            OrderEvent::SubmitAck,
            OrderEvent::Fill,
            OrderEvent::Cancel,
            OrderEvent::Reject,
        ]
    }
}
```

### Running an Instance

`FsmInstance` maintains the active state on the stack and provides in-place updates:

```rust
let mut order = FsmInstance::<OrderMachine>::new();
assert_eq!(order.state(), OrderState::Pending);

assert_eq!(order.send(OrderEvent::Submit), Ok(OrderState::Submitting));
assert_eq!(order.send(OrderEvent::SubmitAck), Ok(OrderState::Open));
assert_eq!(order.send(OrderEvent::Fill), Ok(OrderState::Filled));

// Rejecting an invalid event returns InvalidTransition without modifying state:
let err = order.send(OrderEvent::Submit).unwrap_err();
assert_eq!(order.state(), OrderState::Filled);
```

---

## 2. CLI Code Generation (`xsm codegen`)

Export a statechart from **[Stately Studio](https://stately.ai/editor)** as JSON, and use `xsm codegen` to generate production-ready, idiomatic Rust code:

```bash
# Generate to file
xsm codegen order.json -o src/fsm/order.rs

# Print to stdout
xsm codegen order.json

# Enforce drift gate in CI pipelines:
xsm codegen order.json -o src/fsm/order.rs --check
```

### What `xsm codegen` Generates
1. `#[repr(u8)]` fieldless `State` and `Event` enums.
2. Inlined `next(self, event)` method with `< 5 ns` jump table execution.
3. Implementation of `Fsm` and `ExportXState`.
4. Instance type alias `type OrderInstance = FsmInstance<OrderMachine>;`.
5. Automatic resolution of compound state hierarchies and state inheritance.
6. Embedded `#[cfg(test)]` unit tests verifying every declared edge and asserting rejection of invalid transitions.

---

## 3. Procedural Macro (`xstate_from_json!`)

Embed an XState JSON document directly at compile time with zero runtime JSON overhead:

```rust
use xstate_macros::xstate_from_json;

// Relative path to JSON document:
xstate_from_json!("fixtures/order.json");

// Or inline JSON literal:
xstate_inline_json!(r#"{
    "id": "toggle",
    "initial": "inactive",
    "states": {
        "inactive": { "on": { "TOGGLE": "active" } },
        "active": { "on": { "TOGGLE": "inactive" } }
    }
}"#);

fn main() {
    let mut toggle = ToggleMachine::new_instance();
    assert_eq!(toggle.state(), ToggleState::Inactive);
    toggle.send(ToggleEvent::Toggle).unwrap();
    assert_eq!(toggle.state(), ToggleState::Active);
}
```

### Incremental Recompilation Tracking
When passing a file path to `xstate_from_json!("path/to/chart.json")`, the macro automatically emits an `include_str!` tracking statement so that **Cargo automatically re-expands the macro when the JSON file is modified on disk**.

---

## 4. Bidirectional Stately Studio Round-Trip

Every generated machine implements `ExportXState`:

```rust
// Export native enum machine back to XState JSON:
let value = OrderMachine::to_xstate_json();

// Convert directly into a dynamic xstate-core Machine:
let machine = OrderMachine::to_machine()?;
assert_eq!(machine.id(), "order");
```

This enables designing visually in Stately Studio, generating high-performance native code for hot loops, and visualizing runtime snapshots back in Stately Inspector.
