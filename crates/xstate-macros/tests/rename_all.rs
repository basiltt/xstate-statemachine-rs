//! Every `rename_all` convention for `TypedEvent` and `TypedState`, including acronym and
//! digit word splitting, and per-variant `rename` overrides.

use xstate_core::Event;
use xstate_macros::{TypedEvent, TypedState};

#[derive(Debug, TypedEvent)]
enum Plain {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[derive(Debug, TypedEvent)]
#[event(rename_all = "snake_case")]
enum Snake {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[derive(Debug, TypedEvent)]
#[event(rename_all = "SCREAMING_SNAKE_CASE")]
enum ScreamingSnake {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[derive(Debug, TypedEvent)]
#[event(rename_all = "kebab-case")]
enum Kebab {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[derive(Debug, TypedEvent)]
#[event(rename_all = "camelCase")]
enum Camel {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[derive(Debug, TypedEvent)]
#[event(rename_all = "PascalCase")]
enum Pascal {
    OrderPaid,
    HttpRequestSent,
    Step2Done,
}

#[test]
fn no_rename_all_keeps_the_variant_spelling() {
    assert_eq!(Plain::OrderPaid.event_type(), "OrderPaid");
    assert_eq!(Plain::HttpRequestSent.event_type(), "HttpRequestSent");
    assert_eq!(Plain::Step2Done.event_type(), "Step2Done");
}

#[test]
fn all_conventions_respell_variant_names() {
    assert_eq!(Snake::OrderPaid.event_type(), "order_paid");
    assert_eq!(Snake::HttpRequestSent.event_type(), "http_request_sent");
    assert_eq!(Snake::Step2Done.event_type(), "step2_done");

    assert_eq!(ScreamingSnake::OrderPaid.event_type(), "ORDER_PAID");
    assert_eq!(ScreamingSnake::HttpRequestSent.event_type(), "HTTP_REQUEST_SENT");
    assert_eq!(ScreamingSnake::Step2Done.event_type(), "STEP2_DONE");

    assert_eq!(Kebab::OrderPaid.event_type(), "order-paid");
    assert_eq!(Kebab::HttpRequestSent.event_type(), "http-request-sent");
    assert_eq!(Kebab::Step2Done.event_type(), "step2-done");

    assert_eq!(Camel::OrderPaid.event_type(), "orderPaid");
    assert_eq!(Camel::HttpRequestSent.event_type(), "httpRequestSent");
    assert_eq!(Camel::Step2Done.event_type(), "step2Done");

    assert_eq!(Pascal::OrderPaid.event_type(), "OrderPaid");
    assert_eq!(Pascal::HttpRequestSent.event_type(), "HttpRequestSent");
    assert_eq!(Pascal::Step2Done.event_type(), "Step2Done");
}

/// A per-variant `rename` wins over the enum-level convention, and `r#` prefixes are stripped.
#[derive(Debug, TypedEvent)]
#[event(rename_all = "snake_case")]
enum Overrides {
    /// Kept verbatim.
    #[event(rename = "order.paid")]
    Paid,
    /// A raw-identifier variant.
    r#Type,
}

#[test]
fn variant_rename_overrides_the_convention() {
    assert_eq!(Overrides::Paid.event_type(), "order.paid");
    assert_eq!(Overrides::r#Type.event_type(), "type");
    let event: Event = Overrides::r#Type.into();
    assert_eq!(event.ty, "type");
}

/// `TypedState` defaults to `snake_case` and honours `camelCase` (already covered in
/// `typed_state.rs`); here only the acronym edge is re-checked end to end.
#[derive(Debug, Clone, Copy, PartialEq, TypedState)]
#[state(machine = "door", rename_all = "camelCase")]
enum Door {
    /// `door.httpRequestSent`.
    HttpRequestSent,
}

#[test]
fn typed_state_camel_case_respells_acronyms() {
    assert_eq!(Door::HttpRequestSent.id(), "door.httpRequestSent");
    assert_eq!(Door::from_id("door.httpRequestSent"), Some(Door::HttpRequestSent));
    assert_eq!(Door::from_id("door.http_request_sent"), None);
}
