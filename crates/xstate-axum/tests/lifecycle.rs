//! Probes, the durable-timer scanner and the inspector.

mod common;

use std::time::Duration;

use common::{
    app, call, connect, get, post, registry, serve, ws_handshake, ws_read_close, ws_read_json,
};
use serde_json::json;
use xstate_axum::{
    InspectorOptions, RegisterOptions, RegistryError, StatechartRegistry, inspector_router,
};
use xstate_core::{Machine, MachineLogic};

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn probes_follow_the_lifecycle() {
    let reg = registry(|b| b);
    let probes = reg.probes_router();
    assert_eq!(get(&probes, "/_xsm/health").await.json(), json!({"status": "ok"}));
    let not_ready = get(&probes, "/_xsm/ready").await;
    assert_eq!(
        not_ready.json(),
        json!({"type": "about:blank", "title": "Not Ready", "status": 503})
    );
    reg.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        get(&probes, "/_xsm/ready").await.json(),
        json!({"status": "ready", "residents": 0, "timers": false})
    );
    reg.shutdown().await;
    assert_eq!(get(&probes, "/_xsm/ready").await.status, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_scanner_wakes_persisted_deadlines() {
    let machine = Machine::from_json(
        r#"{"id":"t","initial":"waiting","states":{"waiting":{"after":{"50":"expired"}},"expired":{}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let reg = StatechartRegistry::builder(common::store())
        .run_timers(Duration::from_millis(20))
        .register("t", machine, MachineLogic::dynamic(), RegisterOptions::default())
        .and_then(xstate_axum::RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"));
    reg.start().unwrap_or_else(|e| panic!("{e}"));
    assert!(reg.timers_running());
    reg.act("t", "k1", true, |_| Ok(())).await.unwrap_or_else(|e| panic!("{e}"));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let state = reg.peek("t", "k1").await.unwrap_or_else(|e| panic!("{e}"))["state"].clone();
        if state == "expired" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "timer never fired; state {state}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    reg.shutdown().await;
    assert!(!reg.timers_running());
}

fn inspected(token: &str) -> (std::sync::Arc<StatechartRegistry>, axum::Router) {
    let reg = registry(|b| b);
    let options = InspectorOptions {
        debug: true,
        token: Some(token.to_owned()),
        ..InspectorOptions::default()
    };
    let (inspector, sink) = inspector_router(&reg, options).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sink.token(), token);
    (std::sync::Arc::clone(&reg), app(&reg).merge(inspector))
}

#[tokio::test]
async fn inspector_refuses_to_mount_without_debug() {
    let reg = registry(|b| b);
    let result = inspector_router(&reg, InspectorOptions::default());
    assert!(matches!(result, Err(RegistryError::NotDebug)));
}

#[tokio::test]
async fn inspector_token_host_and_origin_rules() {
    let (_reg, app) = inspected("s3cret");
    let path = "/_xsm/inspect";
    let anonymous = get(&app, path).await;
    assert_eq!(
        anonymous.json(),
        json!({"type": "about:blank", "title": "Unauthorized", "status": 401, "detail": "inspector token"})
    );
    let remote = call(&app, "GET", path, &[("host", "example.com")], None).await;
    assert_eq!(
        (remote.status.as_u16(), remote.json()["detail"].clone()),
        (421, json!("loopback only"))
    );
    let cross = call(&app, "GET", path, &[("origin", "http://evil.example")], None).await;
    assert_eq!(cross.status, 403);
    assert_eq!(get(&app, "/_xsm/inspect?token=wrong").await.status, 401);
    let redeemed = get(&app, "/_xsm/inspect?token=s3cret").await;
    assert_eq!(redeemed.status, 303);
    assert_eq!(redeemed.headers["location"], path);
    let cookie = redeemed.headers["set-cookie"].to_str().unwrap_or_default().to_owned();
    assert!(cookie.starts_with("xsm_inspect=s3cret;") && cookie.contains("HttpOnly"), "{cookie}");
    assert_eq!(redeemed.headers["cache-control"], "no-store");

    assert_eq!(post(&app, "/order/i1/events/PAY", r#"{"amount":1}"#).await.status, 200);
    let backlog = call(&app, "GET", path, &[("cookie", "xsm_inspect=s3cret")], None).await;
    assert_eq!(backlog.status, 200);
    let kinds: Vec<String> = backlog.json().as_array().map_or_else(Vec::new, |messages| {
        messages.iter().map(|m| m["type"].as_str().unwrap_or("").to_owned()).collect()
    });
    assert_eq!(
        kinds,
        ["@xstate.actor", "@xstate.event", "@xstate.snapshot", "@xstate.event", "@xstate.snapshot"]
    );
    let bearer = call(&app, "GET", path, &[("authorization", "Bearer s3cret")], None).await;
    assert_eq!(bearer.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspector_websocket_replays_history_and_streams() {
    let (reg, app) = inspected("tok");
    let addr = serve(app.clone()).await;
    let mut refused = connect(addr).await;
    assert!(ws_handshake(&mut refused, "/_xsm/inspect", &[]).await.contains("101"));
    assert_eq!(ws_read_close(&mut refused).await, xstate_axum::WS_POLICY_VIOLATION);

    post(&app, "/order/i2/events/NOTE", "").await;
    let mut ws = connect(addr).await;
    assert!(
        ws_handshake(&mut ws, "/_xsm/inspect", &[("X-XSM-Token", "tok")]).await.contains("101")
    );
    assert_eq!(ws_read_json(&mut ws).await["type"], "@xstate.actor", "history replayed first");
    for _ in 0..4 {
        ws_read_json(&mut ws).await;
    }
    reg.act("order", "i2", true, |interp| Ok(interp.send("CANCEL").map(|_| ())?))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let live = ws_read_json(&mut ws).await;
    assert_eq!(
        (live["type"].clone(), live["event"]["type"].clone()),
        (json!("@xstate.event"), json!("CANCEL"))
    );
}
