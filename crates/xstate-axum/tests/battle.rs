//! Adversarial HTTP / WebSocket / SSE pass over every public route (QA §6.2 xstate-axum matrix).
//!
//! This is the battle suite on top of `http.rs`, `lifecycle.rs` and `streaming.rs`. It targets the
//! gaps those suites leave open: the full problem-details shape with no secret leakage, the
//! header-only idempotency contract (a body `idempotency_key` must not be trusted), origin/content
//! type edge cases, 408/413, authorization-before-existence, resident persistence, per-event
//! authorization over WebSocket, and build-time option validation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::response::IntoResponse;
use common::{
    app, call, connect, get, order_logic, order_machine, post, registry, registry_over, serve,
    store, ws_handshake, ws_read_close, ws_read_json, ws_send_text,
};
use xstate_axum::{
    ApiError, AuthRequest, Authorizer, Forbidden, IDEMPOTENCY_HEADER, InspectorOptions,
    RegisterOptions, RegistryError, RouterOptions, StatechartRegistry, inspector_router, router,
};
use xstate_persistence::{MemoryInbox, StateStore};

fn version_of(store: &Arc<dyn StateStore>, key: &str) -> Option<u64> {
    store.load(key).unwrap_or_else(|e| panic!("{e}")).map(|r| r.version)
}

fn idempotent_registry(store: Arc<dyn StateStore>) -> Arc<StatechartRegistry> {
    registry_over(store, |b| {
        b.inbox(Arc::new(MemoryInbox::new())).principal(|parts: &axum::http::request::Parts| {
            parts.headers.get("x-user").and_then(|v| v.to_str().ok()).map(str::to_owned)
        })
    })
}

// ------------------------------------------------------------------ problem details (RFC 9457)

/// Every 5xx (and representative 4xx) problem carries a fixed title + class name and never the
/// internal error message; a store/engine message must not leak (paths, keys, payload fragments).
#[test]
fn problem_details_are_fixed_shape_and_never_leak_internals() {
    let cases: Vec<ApiError> = vec![
        ApiError::Internal("C:\\secret\\token=abc".into()),
        ApiError::Store(xstate_persistence::StoreError::Unavailable(
            "redis://user:pass@host".into(),
        )),
        ApiError::Store(xstate_persistence::StoreError::Corrupt("corrupt snapshot body".into())),
        ApiError::Store(xstate_persistence::StoreError::SnapshotTooLarge {
            key: "k".into(),
            size: 10,
            limit: 5,
        }),
        ApiError::Forbidden,
        ApiError::Unauthenticated("who are you".into()),
        ApiError::PayloadTooLarge,
        ApiError::RequestTimeout,
        ApiError::UnsupportedMediaType("not json".into()),
        ApiError::ReservedKey,
        ApiError::IdempotencyNotConfigured,
        ApiError::IdempotencyMismatch,
        ApiError::IdempotencyInFlight,
        ApiError::UnknownMachine("nope".into()),
        ApiError::ShuttingDown,
        ApiError::SnapshotDrift { machine_version: Some("2".into()), message: "drift".into() },
    ];
    for error in cases {
        let body = error.to_problem().body();
        assert_eq!(body["type"], "about:blank", "{error:?}");
        assert!(body["title"].is_string(), "{error:?}");
        assert_eq!(body["status"].as_u64(), Some(u64::from(error.status())), "{error:?}");
        if error.status() >= 500 || error.status() == 503 {
            let text = body.to_string();
            for secret in ["secret", "token=abc", "user:pass", "corrupt snapshot body", "C:\\"] {
                assert!(!text.contains(secret), "leak of {secret:?} in {text}");
            }
            assert!(body["detail"].is_null(), "{error:?} must carry no detail");
        }
    }
    // A problem rendered through IntoResponse keeps the RFC 9457 media type.
    let response = ApiError::Internal("x".into()).into_response();
    assert_eq!(
        response.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

// ------------------------------------------------------------------ idempotency (header only)

/// The adapter owns `idempotency_key`: it is set only from the validated `Idempotency-Key`
/// header, and a body that carries it must be refused (Python `ADAPTER_OWNED_PAYLOAD_KEYS`).
#[tokio::test]
async fn body_idempotency_key_is_refused_like_python() {
    let store = store();
    let app = app(&idempotent_registry(Arc::clone(&store)));
    let body = r#"{"idempotency_key":"k1"}"#;
    let smuggled =
        call(&app, "POST", "/order/o1/events/NOTE", &[("x-user", "alice")], Some(body)).await;
    assert_eq!(
        smuggled.status.as_u16(),
        422,
        "a body idempotency_key must be refused, not trusted as the idempotency key"
    );
    assert_eq!(smuggled.json()["error"], "ReservedKeyError");

    // The header is the only trusted source and still works.
    let via_header = call(
        &app,
        "POST",
        "/order/o1/events/NOTE",
        &[("x-user", "alice"), (IDEMPOTENCY_HEADER, "k2")],
        Some("{}"),
    )
    .await;
    assert_eq!(via_header.status.as_u16(), 200);
    let replay = call(
        &app,
        "POST",
        "/order/o1/events/NOTE",
        &[("x-user", "alice"), (IDEMPOTENCY_HEADER, "k2")],
        Some("{}"),
    )
    .await;
    assert_eq!(replay.json()["duplicate"], true, "the header key is deduplicated");
}

#[test]
fn idempotency_key_header_length_boundary() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(IDEMPOTENCY_HEADER, "k".repeat(255).parse().unwrap_or_else(|e| panic!("{e}")));
    let key = xstate_axum::idempotency_key_from(&headers).ok().flatten().unwrap_or_default();
    assert_eq!(key.len(), 255, "255 characters is accepted");

    headers.insert(IDEMPOTENCY_HEADER, "k".repeat(256).parse().unwrap_or_else(|e| panic!("{e}")));
    assert!(matches!(xstate_axum::idempotency_key_from(&headers), Err(ApiError::BadRequest(_))));
}

// ------------------------------------------------------------------ origin / content type

#[tokio::test]
async fn origin_rules_mixed_case_allow_list_and_missing() {
    // allow-listed origin beats the host; same-origin is case-insensitive; missing Origin is a
    // non-browser client and is admitted.
    let reg = registry_over(store(), |b| b.allowed_origins(["https://app.example"]));
    let app = app(&reg);
    let allow_listed =
        call(&app, "POST", "/order/o1/events/NOTE", &[("origin", "https://app.example")], Some(""))
            .await;
    assert_eq!(allow_listed.status, 200, "allow-listed origin is admitted");

    let mixed_case =
        call(&app, "POST", "/order/o1/events/NOTE", &[("origin", "HTTP://127.0.0.1")], Some(""))
            .await;
    assert_eq!(mixed_case.status, 200, "same-origin is case-insensitive");

    let evil =
        call(&app, "POST", "/order/o1/events/NOTE", &[("origin", "http://evil.example")], Some(""))
            .await;
    assert_eq!(evil.status, 403);
    assert_eq!(evil.json()["title"], "Origin not allowed");

    // Missing Origin (curl, server-to-server) is not a browser: admitted.
    let missing = call(&app, "POST", "/order/o1/events/NOTE", &[], Some("")).await;
    assert_eq!(missing.status, 200);
}

#[tokio::test]
async fn post_content_type_enforcement() {
    let app = app(&registry(|b| b));
    let json_charset = call(
        &app,
        "POST",
        "/order/o1/events/NOTE",
        &[("content-type", "application/json; charset=utf-8")],
        Some(""),
    )
    .await;
    assert_eq!(json_charset.status, 200, "charset parameter is accepted");
    let upper = call(
        &app,
        "POST",
        "/order/o1/events/NOTE",
        &[("content-type", "Application/JSON")],
        Some(""),
    )
    .await;
    assert_eq!(upper.status, 200, "media type is case-insensitive");
    let text =
        call(&app, "POST", "/order/o1/events/NOTE", &[("content-type", "text/plain")], Some(""))
            .await;
    assert_eq!(text.status, 415);
    assert_eq!(text.json()["error"], "UnsupportedMediaTypeError");
    let no_type = call(&app, "POST", "/order/o1/events/NOTE", &[], None).await;
    assert_eq!(no_type.status, 415, "even an empty body needs application/json");
}

// ------------------------------------------------------------------ body limits and timeouts

#[tokio::test]
async fn read_body_answers_408_on_a_stalled_body_and_413_on_an_oversized_chunk() {
    use xstate_axum::body::read_body;
    let stalled = axum::body::Body::from_stream(futures_util::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let timeout = read_body(stalled, 1024, Duration::from_millis(50)).await.err().unwrap();
    assert_eq!((timeout.status(), timeout.to_problem().title.as_str()), (408, "Request Timeout"));

    let big = axum::body::Body::from(vec![b'x'; 100]);
    let too_large = read_body(big, 10, Duration::from_secs(1)).await.err().unwrap();
    assert_eq!(
        (too_large.status(), too_large.to_problem().title.as_str()),
        (413, "Payload Too Large")
    );
}

/// `json_body` (public helper) trusts a declared `Content-Length: 0`; the routes use the internal
/// `bounded_json` which reads the real body, so this is a public-API quirk, not a route hole.
#[tokio::test]
async fn json_body_trusts_a_declared_zero_content_length() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap_or_else(|e| panic!("{e}")));
    headers.insert("content-length", "0".parse().unwrap_or_else(|e| panic!("{e}")));
    let body = axum::body::Body::from(r#"{"a":1}"#);
    let parsed = xstate_axum::json_body(&headers, body, 1024, Duration::from_secs(1))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(parsed.is_empty(), "declared 0 is trusted even though a body arrived");
}

// ------------------------------------------------------------------ keys and authorization order

#[tokio::test]
async fn over_long_instance_key_is_400() {
    let app = app(&registry(|b| b));
    let long = "k".repeat(300);
    let reply = get(&app, &format!("/order/{long}")).await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.json()["error"], "InvalidKeyError");
    assert_eq!(reply.content_type(), "application/problem+json");
}

#[tokio::test]
async fn authorization_runs_before_the_existence_check() {
    let deny: Arc<dyn Authorizer> = Arc::new(|_: &AuthRequest<'_>| Err(Forbidden));
    let reg = StatechartRegistry::builder(store())
        .register("order", order_machine(), order_logic(), RegisterOptions::new(deny))
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    let options = RouterOptions { create_if_missing: false, ..RouterOptions::default() };
    let app = router(&reg, "order", options).unwrap_or_else(|e| panic!("{e}"));
    // An unknown key under a forbidding authorizer is 403, never 404 (existence is not probed).
    assert_eq!(get(&app, "/order/nope").await.status, 403);
    assert_eq!(get(&app, "/order/nope/stream").await.status, 403);
}

#[tokio::test]
async fn unknown_machine_router_is_a_build_error() {
    let reg = registry(|b| b);
    assert!(matches!(
        router(&reg, "nope", RouterOptions::default()),
        Err(RegistryError::UnknownMachine(name)) if name == "nope"
    ));
}

// ------------------------------------------------------------------ lifecycle / persistence

#[tokio::test]
async fn resident_state_survives_a_new_registry_over_the_same_store() {
    let store = store();
    let first = registry_over(Arc::clone(&store), |b| b);
    post(&app(&first), "/order/o1/events/PAY", r#"{"amount":9}"#).await;
    assert_eq!(version_of(&store, "order.o1"), Some(1));

    let second = registry_over(Arc::clone(&store), |b| b);
    assert_eq!(get(&app(&second), "/order/o1").await.json()["state"], "paid");
    let missing = get(&app(&second), "/order/fresh").await;
    assert_eq!(missing.json()["state"], "pending", "an unknown key reports the initial state");
    assert_eq!(version_of(&store, "order.fresh"), None, "a read never creates the instance");
}

// ------------------------------------------------------------------ WebSocket per-event authorization

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_runs_the_authorizer_per_event() {
    let authorize: Arc<dyn Authorizer> = Arc::new(|r: &AuthRequest<'_>| match r.event {
        // Connect / reads, and the one event clients may send.
        None | Some("NOTE") => Ok(()),
        Some(_) => Err(Forbidden),
    });
    let reg = StatechartRegistry::builder(store())
        .register("order", order_machine(), order_logic(), RegisterOptions::new(authorize))
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    let addr = serve(app(&reg)).await;

    // A refused event closes the socket with 1008.
    let mut refused = connect(addr).await;
    assert!(ws_handshake(&mut refused, "/order/w1/ws", &[]).await.contains("101"));
    assert_eq!(ws_read_json(&mut refused).await["kind"], "snapshot");
    ws_send_text(&mut refused, r#"{"type":"PAY","payload":{"amount":1}}"#).await;
    assert_eq!(ws_read_close(&mut refused).await, xstate_axum::WS_POLICY_VIOLATION);

    // An allowed event still flows.
    let mut ok = connect(addr).await;
    assert!(ws_handshake(&mut ok, "/order/w2/ws", &[]).await.contains("101"));
    assert_eq!(ws_read_json(&mut ok).await["kind"], "snapshot");
    ws_send_text(&mut ok, r#"{"type":"NOTE"}"#).await;
    assert_eq!(ws_read_json(&mut ok).await["kind"], "receipt");
}

// ------------------------------------------------------------------ build-time option validation

#[test]
fn invalid_numeric_options_are_rejected_at_build_time() {
    let bad_heartbeat = StatechartRegistry::builder(store()).heartbeat(Duration::ZERO).build();
    assert!(matches!(bad_heartbeat, Err(RegistryError::InvalidSetting(_))), "heartbeat");
    let bad_timeout = StatechartRegistry::builder(store()).body_timeout(Duration::ZERO).build();
    assert!(matches!(bad_timeout, Err(RegistryError::InvalidSetting(_))), "body_timeout");
    let bad_per_key = StatechartRegistry::builder(store()).max_connections_per_key(0).build();
    assert!(
        matches!(bad_per_key, Err(RegistryError::InvalidSetting(_))),
        "max_connections_per_key"
    );
    let bad_timers = StatechartRegistry::builder(store()).run_timers(Duration::ZERO).build();
    assert!(matches!(bad_timers, Err(RegistryError::InvalidSetting(_))), "timer interval");

    // An inbox without a principal resolver is refused (X0.2).
    let no_principal =
        StatechartRegistry::builder(store()).inbox(Arc::new(MemoryInbox::new())).build();
    assert!(matches!(no_principal, Err(RegistryError::InvalidSetting(_))));
}

// ------------------------------------------------------------------ inspector token precedence

#[tokio::test]
async fn inspector_token_precedence_and_rejection() {
    let reg = registry(|b| b);
    let options =
        InspectorOptions { debug: true, token: Some("tok".into()), ..InspectorOptions::default() };
    let (inspector, _sink) = inspector_router(&reg, options).unwrap_or_else(|e| panic!("{e}"));
    let app = app(&reg).merge(inspector);
    let path = "/_xsm/inspect";

    let basic = call(&app, "GET", path, &[("authorization", "Basic tok")], None).await;
    assert_eq!(basic.status, 401, "only Bearer is accepted");

    let bearer_wins =
        call(&app, "GET", path, &[("authorization", "Bearer tok"), ("x-xsm-token", "wrong")], None)
            .await;
    assert_eq!(bearer_wins.status, 200, "Bearer takes precedence over the token header");

    let header_tok = call(&app, "GET", path, &[("x-xsm-token", "tok")], None).await;
    assert_eq!(header_tok.status, 200);

    let cookie_wrong = call(&app, "GET", path, &[("cookie", "xsm_inspect=wrong")], None).await;
    assert_eq!(cookie_wrong.status, 401);
    let cookie_ok = call(&app, "GET", path, &[("cookie", "xsm_inspect=tok")], None).await;
    assert_eq!(cookie_ok.status, 200);
}
