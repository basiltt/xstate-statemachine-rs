//! `#[derive(TypedEvent)]`: naming, payload round trips and the chart check.

use serde::{Deserialize, Serialize};
use serde_json::json;
use xstate_core::{ErrorKind, Event, MachineLogic, SyncInterpreter, TypedEvent};
use xstate_macros::{TypedEvent, statechart};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    sku: String,
    qty: u32,
}

#[derive(Debug, Clone, PartialEq, TypedEvent)]
#[event(rename_all = "SCREAMING_SNAKE_CASE")]
enum OrderEvent {
    Checkout,
    AddItem(Item),
    SetNote {
        text: String,
        urgent: Option<bool>,
    },
    #[event(rename = "order.paid")]
    Paid {
        amount: f64,
        r#type: String,
    },
    Retry(),
}

#[test]
fn event_types_follow_rename_rules() {
    assert_eq!(OrderEvent::Checkout.event_type(), "CHECKOUT");
    assert_eq!(OrderEvent::Retry().event_type(), "RETRY");
    let note = OrderEvent::SetNote { text: "x".into(), urgent: None };
    assert_eq!(note.event_type(), "SET_NOTE");
    assert_eq!(
        OrderEvent::declared_events(),
        ["ADD_ITEM", "CHECKOUT", "RETRY", "SET_NOTE", "order.paid"]
    );
    // The trait view agrees with the inherent items.
    assert_eq!(<OrderEvent as TypedEvent>::event_type(&OrderEvent::Checkout), "CHECKOUT");
    assert_eq!(<OrderEvent as TypedEvent>::declared_events().len(), 5);
}

#[test]
fn payloads_round_trip_through_event() {
    let cases = [
        (OrderEvent::Checkout, serde_json::Value::Null),
        (OrderEvent::AddItem(Item { sku: "a1".into(), qty: 2 }), json!({ "sku": "a1", "qty": 2 })),
        (
            OrderEvent::SetNote { text: "fragile".into(), urgent: Some(true) },
            json!({ "text": "fragile", "urgent": true }),
        ),
        (
            OrderEvent::Paid { amount: 9.5, r#type: "card".into() },
            json!({ "amount": 9.5, "type": "card" }),
        ),
    ];
    for (typed, payload) in cases {
        let event: Event = typed.clone().into();
        assert_eq!(event.ty, typed.event_type());
        assert_eq!(event.payload, payload);
        assert_eq!(OrderEvent::try_from(event).unwrap(), typed);
    }
}

#[test]
fn decoding_is_lenient_about_absent_options_and_strict_about_shapes() {
    let missing_option = Event::with_payload("SET_NOTE", json!({ "text": "t" }));
    assert_eq!(
        OrderEvent::try_from(missing_option).unwrap(),
        OrderEvent::SetNote { text: "t".into(), urgent: None }
    );
    let wrong_shape = Event::with_payload("ADD_ITEM", json!({ "sku": 1 }));
    let err = OrderEvent::try_from(wrong_shape).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidEvent);
    assert!(err.to_string().contains("ADD_ITEM"), "{err}");
    let missing_field = Event::with_payload("order.paid", json!({ "amount": 1.0 }));
    let err = OrderEvent::try_from(missing_field).unwrap_err();
    assert!(err.to_string().contains("field `type`"), "{err}");
    let unknown = OrderEvent::try_from(Event::new("NOPE")).unwrap_err();
    assert!(unknown.to_string().contains("unknown event `NOPE`"), "{unknown}");
}

#[test]
fn check_reports_both_directions_and_honours_wildcards() {
    let exact = statechart! {
        id: "order",
        initial: cart,
        states: {
            cart {
                on {
                    ADD_ITEM => none, SET_NOTE => none, CHECKOUT => paying, RETRY => none,
                },
            }
            paying { on { "order.paid" => done } }
            done { type: final }
        }
    }
    .unwrap();
    assert_eq!(OrderEvent::check(&exact), Ok(()));

    let drifted = statechart! {
        id: "order",
        initial: cart,
        states: {
            cart { on { ADD_ITEM => none, CHECKOUT => paying, CANCEL => done } }
            paying { on { order.* => done } }
            done { type: final }
        }
    }
    .unwrap();
    let report = OrderEvent::check(&drifted).unwrap_err();
    assert_eq!(
        report,
        "machine `order`: events unhandled by the chart: RETRY, SET_NOTE; \
         events missing from the enum: CANCEL"
    );
}

#[test]
fn typed_events_drive_an_interpreter() {
    let machine = statechart! {
        id: "order",
        initial: cart,
        states: { cart { on { CHECKOUT => paying } }, paying { on { "order.paid" => done } }, done { type: final } }
    }
    .unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().unwrap();
    interp.send(OrderEvent::Checkout).unwrap();
    interp.send(OrderEvent::Paid { amount: 1.0, r#type: "cash".into() }).unwrap();
    assert_eq!(interp.state_ids(), ["order.done"]);
}

/// A public enum: the generated inherent items are public and documented too.
#[derive(Debug, TypedEvent)]
#[event(rename_all = "kebab-case")]
pub enum Kebab {
    /// `http-request-sent`.
    HttpRequestSent,
    /// `custom`, carrying a bare number.
    #[event(rename = "custom")]
    Other(u8),
}

#[test]
fn kebab_case_and_public_enums() {
    assert_eq!(Kebab::HttpRequestSent.event_type(), "http-request-sent");
    assert_eq!(Kebab::Other(1).event_type(), "custom");
    let event: Event = Kebab::Other(7).into();
    assert_eq!(event.payload, json!(7));
}

#[derive(Debug, TypedEvent)]
enum Never {}

#[test]
fn an_empty_enum_declares_nothing_and_accepts_nothing() {
    assert_eq!(Never::declared_events(), [] as [&str; 0]);
    assert!(Never::try_from(Event::new("ANY")).is_err());
}
