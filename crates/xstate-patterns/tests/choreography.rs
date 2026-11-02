//! `ChoreographyRouter` over a fake bus: two machines, each owning its own instance of the
//! same business key, unknown types acked, loop guard.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use xstate_core::{Machine, MachineLogic};
use xstate_eda::dispatcher::{Dispatch, HandlerError};
use xstate_eda::{Envelope, SyncBrokerAdapter, SyncFakeBrokerAdapter};
use xstate_patterns::{
    ChoreographyError, ChoreographyRouter, PersistedHandler, Route, instance_key,
};
use xstate_persistence::{MemoryStore, ReceiptJson, StateStore};

const NONE: Duration = Duration::ZERO;

fn order() -> Machine {
    Machine::from_json(
        r#"{"id":"order","initial":"placed","states":{
            "placed":{"on":{"PAID":"paid"}},
            "paid":{"on":{"SHIPPED":"done"}},
            "done":{"type":"final"}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn shipping() -> Machine {
    Machine::from_json(
        r#"{"id":"shipping","initial":"waiting","states":{
            "waiting":{"on":{"PAID":"packing"}},
            "packing":{"on":{"xsm.other.DISPATCH":"sent"}},
            "sent":{}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn routes() -> HashMap<String, Route> {
    HashMap::from([
        ("xsm.order.PAID".to_owned(), Route::new(order())),
        ("xsm.order.SHIPPED".to_owned(), Route::new(order())),
        ("payment.settled".to_owned(), Route::as_event(shipping(), "PAID")),
    ])
}

fn envelope(ty: &str, subject: &str) -> Envelope {
    Envelope::new(ty)
        .subject(subject)
        .data(json!({"amount": 5}))
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn routes_by_type_into_per_machine_instances() {
    let store = Arc::new(MemoryStore::new());
    let handler = PersistedHandler::new(store.clone(), |_| MachineLogic::dynamic());
    let router = ChoreographyRouter::new(routes(), handler);
    let bus = SyncFakeBrokerAdapter::new();
    for ty in ["xsm.order.PAID", "payment.settled", "xsm.order.SHIPPED", "inventory.counted"] {
        bus.publish("events", &envelope(ty, "o-1")).unwrap_or_else(|e| panic!("{e}"));
    }

    let result = router.run_until_quiet(&bus, NONE, 10).unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(result.processed, 3);
    assert_eq!(result.ignored, 1, "a type nobody routes is acked, not dead-lettered");
    assert_eq!(result.dead_lettered, 0);
    let mut keys = store.list_keys("", 10).unwrap_or_else(|e| panic!("{e}"));
    keys.sort();
    assert_eq!(keys, ["order:o-1", "shipping:o-1"], "one business key, one instance per machine");
    let order_snapshot = store
        .load("order:o-1")
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("saved"));
    assert!(order_snapshot.snapshot.contains("\"order.done\""));
    let shipping_snapshot = store
        .load("shipping:o-1")
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("saved"));
    assert!(
        shipping_snapshot.snapshot.contains("\"shipping.packing\""),
        "route event PAID applied"
    );
}

#[test]
fn event_names_follow_the_route_or_the_default() {
    let router = ChoreographyRouter::new(
        routes(),
        |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            unreachable!("not dispatched in this test")
        },
    );
    assert_eq!(router.event_for(&envelope("xsm.order.PAID", "x")), "PAID");
    assert_eq!(router.event_for(&envelope("payment.settled", "x")), "PAID");
    assert_eq!(router.event_for(&envelope("inventory.counted", "x")), "inventory.counted");
    assert_eq!(router.machine_for("payment.settled").map(Machine::id), Some("shipping"));
    assert!(router.machine_for("nope").is_none());
    let no_subject = Envelope::new("t").build().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(instance_key(&no_subject, &order()), "order:None");
}

#[test]
fn a_finished_instance_dead_letters_instead_of_retrying() {
    let store = Arc::new(MemoryStore::new());
    let handler = PersistedHandler::new(store, |_| MachineLogic::dynamic());
    let router = ChoreographyRouter::new(routes(), handler);
    let bus = SyncFakeBrokerAdapter::new();
    for ty in ["xsm.order.PAID", "xsm.order.SHIPPED", "xsm.order.PAID"] {
        bus.publish("events", &envelope(ty, "o-2")).unwrap_or_else(|e| panic!("{e}"));
    }
    let result = router.run_until_quiet(&bus, NONE, 10).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(result.processed, 2);
    assert_eq!(result.dead_lettered, 1);
    assert!(
        result.outcomes.iter().any(|(_, o)| o == "dead_lettered:instance_done"),
        "{:?}",
        result.outcomes
    );
}

#[test]
fn loop_guard_reports_an_unsettled_choreography() {
    let bus = SyncFakeBrokerAdapter::new();
    let chatty = |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
        // every handled event publishes another one: never quiet
        bus.publish("events", &envelope("xsm.order.PAID", d.key))
            .map_err(|e| HandlerError::failed("BrokerError", e.to_string()))?;
        Ok(ReceiptJson {
            state_ids: vec![],
            changed: true,
            error: None,
            deferred: false,
            denied: false,
            duplicate: false,
        })
    };
    let router = ChoreographyRouter::new(routes(), chatty);
    bus.publish("events", &envelope("xsm.order.PAID", "o-3")).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(router.run_until_quiet(&bus, NONE, 3), Err(ChoreographyError::Unsettled(3))));
    assert!(matches!(
        router.run_until_quiet(&bus, NONE, 0),
        Err(ChoreographyError::InvalidRounds(0))
    ));
}
