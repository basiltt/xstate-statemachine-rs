# Recipe: Traffic Light Controller

The classic traffic light demonstrates sequential transitions, entry/exit actions, and illegal event rejection.

---

## Statechart Topology

```mermaid
stateDiagram-v2
    [*] --> Green
    Green --> Yellow: TIMER
    Yellow --> Red: TIMER
    Red --> Green: TIMER
    Red --> BlinkingRed: FAULT
    BlinkingRed --> Red: RESET
```

---

## Rust Implementation

```rust
use xstate_statemachine::prelude::*;

fn create_traffic_light() -> Result<SyncInterpreter, xstate_statemachine::core::Error> {
    let machine = Machine::from_json(r#"{
        "id": "traffic_light",
        "initial": "green",
        "context": {"cycles": 0},
        "states": {
            "green": {
                "entry": "logGreen",
                "on": {"TIMER": "yellow"}
            },
            "yellow": {
                "on": {"TIMER": "red"}
            },
            "red": {
                "entry": ["logRed", "incrementCycle"],
                "on": {
                    "TIMER": "green",
                    "FAULT": "blinking_red"
                }
            },
            "blinking_red": {
                "on": {"RESET": "red"}
            }
        }
    }"#)?;

    let logic = MachineLogic::dynamic()
        .action("logGreen", |_scope| {
            println!("Traffic signal is GREEN: Traffic flowing.");
            Ok(())
        })
        .action("logRed", |_scope| {
            println!("Traffic signal is RED: Full stop.");
            Ok(())
        })
        .action("incrementCycle", |scope| {
            let cycles = scope.context["cycles"].as_u64().unwrap_or(0) + 1;
            scope.context["cycles"] = cycles.into();
            Ok(())
        });

    let mut interpreter = SyncInterpreter::new(machine, logic);
    interpreter.start()?;
    Ok(interpreter)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut light = create_traffic_light()?;
    assert_eq!(light.state_ids(), ["traffic_light.green"]);

    // Transition Green -> Yellow -> Red
    light.send("TIMER")?;
    assert_eq!(light.state_ids(), ["traffic_light.yellow"]);

    light.send("TIMER")?;
    assert_eq!(light.state_ids(), ["traffic_light.red"]);
    assert_eq!(light.context()["cycles"], 1);

    // Illegal events for the active state are safely ignored without panicking
    let invalid_receipt = light.send("UNKNOWN_EVENT")?;
    assert!(invalid_receipt.unhandled);
    assert_eq!(light.state_ids(), ["traffic_light.red"]);

    // Fault condition
    light.send("FAULT")?;
    assert_eq!(light.state_ids(), ["traffic_light.blinking_red"]);

    // Reset back to operating state
    light.send("RESET")?;
    assert_eq!(light.state_ids(), ["traffic_light.red"]);

    Ok(())
}
```

---

## Key Takeaways
- **Deterministic Zero-Allocation Execution:** The `TIMER` loop executes in ~106 nanoseconds per step.
- **Unhandled Event Safety:** Events not defined for the current state return a `Receipt` with `unhandled: true` rather than crashing the system.
- **Context Integrity:** Action scopes provide mutable references to context, guaranteeing that state updates are atomically recorded with the macrostep.
