//! Integration tests for `xstate_from_json!` and `xstate_inline_json!` macros.

use xstate_core::fsm::{ExportXState, Fsm};
use xstate_macros::{xstate_from_json, xstate_inline_json};

xstate_inline_json!(
    r#"{
    "id": "door",
    "initial": "closed",
    "states": {
        "closed": { "on": { "OPEN": "opened" } },
        "opened": { "on": { "CLOSE": "closed" } }
    }
}"#
);

#[test]
fn test_inline_json_macro_expansion() {
    let mut door = DoorMachine::new_instance();
    assert_eq!(door.state(), DoorState::Closed);
    assert_eq!(door.send(DoorEvent::Open), Ok(DoorState::Opened));
    assert_eq!(door.state(), DoorState::Opened);
    assert_eq!(door.send(DoorEvent::Close), Ok(DoorState::Closed));
    assert_eq!(door.state(), DoorState::Closed);

    // Invalid transition
    let err = door.send(DoorEvent::Close).unwrap_err();
    assert_eq!(err.state, DoorState::Closed);
    assert_eq!(err.event, DoorEvent::Close);

    // Initial and all states
    assert_eq!(DoorMachine::initial(), DoorState::Closed);
    assert_eq!(DoorMachine::all_states(), &[DoorState::Closed, DoorState::Opened]);
    assert_eq!(DoorMachine::all_events(), &[DoorEvent::Close, DoorEvent::Open]);

    // XState JSON export
    let json = DoorMachine::to_xstate_json();
    assert_eq!(json["id"], "door");
    assert_eq!(json["initial"], "closed");

    // Convert to dynamic Machine
    let dynamic_machine = DoorMachine::to_machine().expect("dynamic machine");
    assert_eq!(dynamic_machine.id(), "door");
}

xstate_from_json!(
    r##"{
    "id": "order_flow",
    "initial": "pending",
    "states": {
        "pending": { "on": { "SUBMIT": "processing" } },
        "processing": {
            "initial": "validating",
            "states": {
                "validating": { "on": { "VALID": "charging" } },
                "charging": { "on": { "CHARGED": "#order_flow.completed" } }
            }
        },
        "completed": { "type": "final" }
    }
}"##
);

#[test]
fn test_hierarchical_expansion() {
    let mut order = OrderFlowMachine::new_instance();
    assert_eq!(order.state(), OrderFlowState::Pending);

    // Entering processing resolves to initial child: ProcessingValidating
    assert_eq!(order.send(OrderFlowEvent::Submit), Ok(OrderFlowState::ProcessingValidating));
    assert_eq!(order.send(OrderFlowEvent::Valid), Ok(OrderFlowState::ProcessingCharging));
    assert_eq!(order.send(OrderFlowEvent::Charged), Ok(OrderFlowState::Completed));
}

xstate_from_json!("fixtures/traffic.json");

#[test]
fn test_from_file_macro_expansion() {
    let mut light = TrafficLightMachine::new_instance();
    assert_eq!(light.state(), TrafficLightState::Red);
    assert_eq!(light.send(TrafficLightEvent::Next), Ok(TrafficLightState::Green));
    assert_eq!(light.send(TrafficLightEvent::Next), Ok(TrafficLightState::Yellow));
    assert_eq!(light.send(TrafficLightEvent::Next), Ok(TrafficLightState::Red));
}
