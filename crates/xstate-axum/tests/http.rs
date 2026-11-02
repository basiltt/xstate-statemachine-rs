//! REST surface: state, sends, receipts, problems, body limits, gates, idempotency.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{app, call, get, order_logic, order_machine, post, registry, store};
use serde_json::{Value, json};
use xstate_axum::{
    ApiError, AuthRequest, EventGate, Forbidden, Problem, RegisterOptions, RegistryError,
    RouterOptions, SendRequest, StatechartRegistry, router,
};
use xstate_persistence::{MemoryInbox, StateStore};

fn version_of(store: &Arc<dyn StateStore>, key: &str) -> Option<u64> {
    store.load(key).unwrap_or_else(|e| panic!("{e}")).map(|r| r.version)
}

#[tokio::test]
async fn get_reports_the_initial_state_without_saving() {
    let store = store();
    let reg = common::registry_over(Arc::clone(&store), |b| b);
    let reply = get(&app(&reg), "/order/o1").await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json(),
        json!({"state": "pending", "state_ids": ["order.pending"],
               "available_events": ["CANCEL", "NOTE"], "machine_version": null})
    );
    assert_eq!(version_of(&store, "order.o1"), None, "a read never creates the instance");
}

#[tokio::test]
async fn send_transitions_and_returns_the_receipt_shape() {
    let store = store();
    let reg = common::registry_over(Arc::clone(&store), |b| b);
    let app = app(&reg);
    let reply = post(&app, "/order/o1/send", r#"{"type":"PAY","payload":{"amount":5}}"#).await;
    assert_eq!(reply.status, 200);
    let body = reply.json();
    let keys: Vec<&String> = body.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "state",
            "state_ids",
            "available_events",
            "changed",
            "denied",
            "deferred",
            "duplicate",
            "error"
        ]
    );
    assert_eq!(body["state"], "paid");
    assert_eq!(body["state_ids"], json!(["order.paid"]));
    assert_eq!(body["available_events"], json!(["SHIP"]));
    assert_eq!((body["changed"].clone(), body["error"].clone()), (json!(true), Value::Null));
    assert_eq!(version_of(&store, "order.o1"), Some(1));
    assert_eq!(get(&app, "/order/o1").await.json()["state"], "paid");
}

#[tokio::test]
async fn guard_refusal_is_denied_409_and_finished_instance_is_409() {
    let app = app(&registry(|b| b));
    let denied = post(&app, "/order/o1/events/PAY", r#"{"amount":0}"#).await;
    assert_eq!(denied.status, 409);
    assert_eq!(
        (denied.json()["denied"].clone(), denied.json()["changed"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(post(&app, "/order/o1/events/CANCEL", "").await.status, 200);
    let stopped = post(&app, "/order/o1/events/PAY", r#"{"amount":1}"#).await;
    assert_eq!(stopped.status, 409);
    assert_eq!(stopped.json()["error"], "InterpreterStoppedError");
    assert_eq!(stopped.json()["state"], "cancelled");
}

#[tokio::test]
async fn no_op_send_is_not_saved() {
    let store = store();
    let reg = common::registry_over(Arc::clone(&store), |b| b);
    let app = app(&reg);
    post(&app, "/order/o1/events/PAY", r#"{"amount":5}"#).await;
    let reply = post(&app, "/order/o1/send", r#"{"type":"CANCEL"}"#).await;
    assert_eq!((reply.status.as_u16(), reply.json()["changed"].clone()), (200, json!(false)));
    assert_eq!(
        version_of(&store, "order.o1"),
        Some(1),
        "an unhandled event does not bump the version"
    );
}

#[tokio::test]
async fn undeclared_event_is_a_validation_problem() {
    let app = app(&registry(|b| b));
    let reply = post(&app, "/order/o1/send", r#"{"type":"NOPE","extra":1}"#).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.content_type(), "application/problem+json");
    assert_eq!(
        reply.json(),
        json!({"type": "about:blank", "title": "Request validation failed", "status": 422,
               "errors": [{"loc": ["body", "type"], "type": "literal_error"},
                          {"loc": ["body", "extra"], "type": "extra_forbidden"}]})
    );
    let missing = call(&app, "POST", "/order/o1/send", &[], None).await;
    assert_eq!(missing.json()["errors"], json!([{"loc": ["body"], "type": "missing"}]));
    let broken = post(&app, "/order/o1/send", "{not json").await;
    assert_eq!(broken.json()["errors"], json!([{"loc": ["body"], "type": "json_invalid"}]));
    let list = post(&app, "/order/o1/events/NOTE", "[1]").await;
    assert_eq!(
        (list.status.as_u16(), list.json()["errors"][0]["type"].clone()),
        (422, json!("dict_type"))
    );
}

#[tokio::test]
async fn strict_registration_refuses_unknown_events_with_422() {
    let reg = StatechartRegistry::builder(store())
        .register(
            "order",
            order_machine(),
            order_logic(),
            RegisterOptions { strict: true, ..RegisterOptions::default() },
        )
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    let error = reg.send(SendRequest::new("order", "o1", "NOPE")).await.err();
    let problem = error.as_ref().map_or_else(|| panic!("accepted"), ApiError::to_problem);
    assert_eq!(
        problem.body(),
        json!({"type": "about:blank", "title": "Unknown event", "status": 422, "error": "UnknownEventError"})
    );
}

#[tokio::test]
async fn problem_shapes_for_reserved_keys_size_and_media_type() {
    let reg = registry(|b| b.max_body_bytes(64));
    let app = app(&reg);
    let reserved = post(&app, "/order/o1/send", r#"{"type":"NOTE","payload":{"wait":true}}"#).await;
    assert_eq!(
        reserved.json(),
        json!({"type": "about:blank", "title": "Reserved key in payload", "status": 422, "error": "ReservedKeyError"})
    );
    let big = format!(r#"{{"type":"NOTE","payload":{{"x":"{}"}}}}"#, "y".repeat(100));
    let too_large = post(&app, "/order/o1/send", &big).await;
    assert_eq!(
        (too_large.status.as_u16(), too_large.json()["title"].clone()),
        (413, json!("Payload Too Large"))
    );
    let text =
        call(&app, "POST", "/order/o1/send", &[("content-type", "text/plain")], Some("hi")).await;
    assert_eq!(text.status, 415);
    assert_eq!(text.json()["title"], "Request body must be application/json");
    assert_eq!(text.json()["error"], "UnsupportedMediaTypeError");
}

fn gated_router(reg: &Arc<StatechartRegistry>) -> axum::Router {
    let gate: Arc<dyn EventGate> = Arc::new(|parts: &axum::http::request::Parts| {
        if parts.headers.contains_key("x-gate") {
            Ok(())
        } else {
            Err(Problem::new(403, "Gate closed"))
        }
    });
    let options = RouterOptions {
        per_event_gates: HashMap::from([("PAY".to_owned(), gate)]),
        ..RouterOptions::default()
    };
    router(reg, "order", options).unwrap_or_else(|e| panic!("{e}"))
}

#[tokio::test]
async fn per_event_gate_is_refused_on_send_and_enforced_on_its_route() {
    let reg = registry(|b| b);
    let app = gated_router(&reg);
    let via_send = post(&app, "/order/o1/send", r#"{"type":"PAY","payload":{"amount":1}}"#).await;
    assert_eq!(
        (via_send.status.as_u16(), via_send.json()["title"].clone()),
        (403, json!("Use this event's dedicated route"))
    );
    let closed = post(&app, "/order/o1/events/PAY", r#"{"amount":1}"#).await;
    assert_eq!(closed.json()["title"], "Gate closed");
    let open =
        call(&app, "POST", "/order/o1/events/PAY", &[("x-gate", "1")], Some(r#"{"amount":1}"#))
            .await;
    assert_eq!((open.status.as_u16(), open.json()["state"].clone()), (200, json!("paid")));
    let gate: Arc<dyn EventGate> = Arc::new(|_: &axum::http::request::Parts| Ok(()));
    let bad = RouterOptions {
        per_event_gates: HashMap::from([("TYPO".to_owned(), gate)]),
        ..RouterOptions::default()
    };
    assert!(matches!(router(&reg, "order", bad), Err(RegistryError::UnknownGatedEvents { .. })));
}

fn idempotent_registry(store: Arc<dyn StateStore>) -> Arc<StatechartRegistry> {
    common::registry_over(store, |b| {
        b.inbox(Arc::new(MemoryInbox::new())).principal(|parts: &axum::http::request::Parts| {
            parts.headers.get("x-user").and_then(|v| v.to_str().ok()).map(str::to_owned)
        })
    })
}

#[tokio::test]
async fn idempotent_replay_returns_the_cached_receipt() {
    let store = store();
    let app = app(&idempotent_registry(Arc::clone(&store)));
    let headers = [("x-user", "alice"), ("idempotency-key", "k1")];
    let first = call(&app, "POST", "/order/o1/events/PAY", &headers, Some(r#"{"amount":5}"#)).await;
    assert_eq!((first.status.as_u16(), first.json()["duplicate"].clone()), (200, json!(false)));
    let replay =
        call(&app, "POST", "/order/o1/events/PAY", &headers, Some(r#"{"amount":5}"#)).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        (replay.json()["duplicate"].clone(), replay.json()["state"].clone()),
        (json!(true), json!("paid"))
    );
    assert_eq!(replay.json()["changed"], true, "the cached receipt describes the original outcome");
    assert_eq!(version_of(&store, "order.o1"), Some(1), "a replay is not saved");
    let conflict =
        call(&app, "POST", "/order/o1/events/PAY", &headers, Some(r#"{"amount":6}"#)).await;
    assert_eq!(
        conflict.json(),
        json!({"type": "about:blank", "title": "Idempotency key reused", "status": 422, "error": "IdempotencyMismatchError"})
    );
    let other_user = [("x-user", "bob"), ("idempotency-key", "k1")];
    let bob = call(&app, "POST", "/order/o2/events/NOTE", &other_user, None).await;
    assert_eq!(bob.json()["duplicate"], false, "keys are scoped per principal and instance");
}

#[tokio::test]
async fn idempotency_key_rules() {
    let app = app(&idempotent_registry(store()));
    let anonymous =
        call(&app, "POST", "/order/o1/events/NOTE", &[("idempotency-key", "k")], None).await;
    assert_eq!(
        (anonymous.status.as_u16(), anonymous.json()["error"].clone()),
        (401, json!("UnauthenticatedError"))
    );
    let long = "x".repeat(300);
    let malformed = call(
        &app,
        "POST",
        "/order/o1/events/NOTE",
        &[("x-user", "a"), ("idempotency-key", &long)],
        None,
    )
    .await;
    assert_eq!(
        (malformed.status.as_u16(), malformed.json()["title"].clone()),
        (400, json!("Invalid Idempotency-Key header"))
    );
    let plain = app_without_inbox();
    let refused =
        call(&plain, "POST", "/order/o1/events/NOTE", &[("idempotency-key", "k")], None).await;
    assert_eq!(refused.status, 501);
    assert_eq!(refused.json()["error"], "IdempotencyNotConfiguredError");
}

fn app_without_inbox() -> axum::Router {
    app(&registry(|b| b))
}

#[tokio::test]
async fn events_list_and_diagram() {
    let app = app(&registry(|b| b));
    let events = get(&app, "/order/o1/events").await.json();
    assert_eq!(events["available"], json!(["CANCEL", "NOTE"]));
    let declared: Vec<Value> = ["CANCEL", "NOTE", "PAY", "SHIP"]
        .iter()
        .map(|e| json!({"type": e, "schema": null}))
        .collect();
    assert_eq!(events["declared"], Value::from(declared));
    let diagram = get(&app, "/order/o1/diagram.mmd").await;
    assert_eq!(diagram.content_type(), "text/plain; charset=utf-8");
    assert!(diagram.text().starts_with("stateDiagram-v2"), "{}", diagram.text());
}

#[tokio::test]
async fn authorizer_sees_principal_and_event() {
    let authorize = Arc::new(|r: &AuthRequest<'_>| match (r.principal, r.event) {
        (Some("admin"), _) | (_, None) => Ok(()),
        _ => Err(Forbidden),
    });
    let reg = StatechartRegistry::builder(store())
        .principal(|p: &axum::http::request::Parts| {
            p.headers.get("x-user").and_then(|v| v.to_str().ok()).map(str::to_owned)
        })
        .register(
            "order",
            order_machine(),
            order_logic(),
            RegisterOptions { authorize, ..RegisterOptions::default() },
        )
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    let app = app(&reg);
    assert_eq!(get(&app, "/order/o1").await.status, 200);
    let refused = post(&app, "/order/o1/events/NOTE", "").await;
    assert_eq!(
        refused.json(),
        json!({"type": "about:blank", "title": "Forbidden", "status": 403, "error": "ForbiddenError"})
    );
    let admin = call(&app, "POST", "/order/o1/events/NOTE", &[("x-user", "admin")], None).await;
    assert_eq!(admin.status, 200);
}

#[tokio::test]
async fn create_if_missing_false_answers_404() {
    let reg = registry(|b| b);
    let options = RouterOptions { create_if_missing: false, ..RouterOptions::default() };
    let app = router(&reg, "order", options).unwrap_or_else(|e| panic!("{e}"));
    let missing = get(&app, "/order/nope").await;
    assert_eq!(
        missing.json(),
        json!({"type": "about:blank", "title": "Not Found", "status": 404, "error": "KeyNotFoundError"})
    );
    assert_eq!(post(&app, "/order/nope/events/NOTE", "").await.status, 404);
    assert_eq!(get(&app, "/order/nope/stream").await.status, 404);
}

#[tokio::test]
async fn context_serializer_drops_private_keys_and_act_publishes() {
    let machine = xstate_core::Machine::from_json(
        r#"{"id":"c","initial":"a","context":{"n":1,"_xsm_errors":[]},"states":{"a":{"on":{"GO":"b"}},"b":{}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let serializer: xstate_axum::ContextSerializer = Arc::new(Value::clone);
    let options =
        RegisterOptions { context_serializer: Some(serializer), ..RegisterOptions::default() };
    let reg = StatechartRegistry::builder(store())
        .register("c", machine, xstate_core::MachineLogic::dynamic(), options)
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    let app = router(&reg, "c", RouterOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(get(&app, "/c/k").await.json()["context"], json!({"n": 1}));
    let mut subscription = reg.subscribers().subscribe("c", "k");
    let state = reg
        .act("c", "k", true, |interp| Ok(interp.send("GO")?.state_ids()))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(state, ["c.b"]);
    match subscription.recv().await {
        xstate_axum::FanoutItem::Transition { seq, body } => {
            assert_eq!(
                (seq, body["state"].clone(), body["context"].clone()),
                (1, json!("b"), json!({"n": 1}))
            );
        }
        other @ xstate_axum::FanoutItem::Closed => panic!("unexpected {other:?}"),
    }
}
