//! SSE and WebSocket streaming against a real server, and the shutdown drain.

mod common;

use std::time::Duration;

use common::{
    app, connect, eventually, get, post, read_until, registry, serve, ws_handshake, ws_read_close,
    ws_read_json, ws_send_text,
};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use xstate_axum::{WS_MESSAGE_TOO_BIG, WS_POLICY_VIOLATION};

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_streams_snapshot_then_transition_and_heartbeats() {
    let reg = registry(|b| b.heartbeat(Duration::from_millis(100)));
    let app = app(&reg);
    let addr = serve(app.clone()).await;
    let mut stream = connect(addr).await;
    let request = "GET /order/s1/stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap_or_else(|e| panic!("{e}"));
    let mut buf = String::new();
    read_until(&mut stream, &mut buf, "event: snapshot", WAIT).await;
    assert!(buf.starts_with("HTTP/1.1 200"), "{buf}");
    assert!(buf.to_ascii_lowercase().contains("content-type: text/event-stream"), "{buf}");
    assert!(buf.contains("id: 0\nevent: snapshot\ndata: {\"state\":\"pending\""), "{buf}");
    assert_eq!(reg.connections(Some(("order", "s1"))), 1);

    let reply = post(&app, "/order/s1/events/PAY", r#"{"amount":3}"#).await;
    assert_eq!(reply.status, 200);
    read_until(&mut stream, &mut buf, "event: transition", WAIT).await;
    read_until(&mut stream, &mut buf, "\n\n", WAIT).await;
    let frame = &buf[buf.find("id: 1\nevent: transition").unwrap_or_else(|| panic!("{buf}"))..];
    assert!(frame.contains("\"state\":\"paid\""), "{frame}");
    assert!(frame.contains("\"changed\":true"), "{frame}");
    read_until(&mut stream, &mut buf, ": heartbeat", WAIT).await;

    drop(stream);
    eventually(WAIT, || reg.connections(None) == 0 && reg.subscribers().count(None) == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_refusals_are_problems() {
    let reg = registry(|b| b.max_connections_per_key(1));
    let app = app(&reg);
    let addr = serve(app.clone()).await;
    let cross =
        common::call(&app, "GET", "/order/s1/stream", &[("origin", "http://evil.example")], None)
            .await;
    assert_eq!(
        (cross.status.as_u16(), cross.json()["title"].clone()),
        (403, json!("Origin not allowed"))
    );
    // Same-origin is admitted (the stream never ends, so it is read over a socket).
    let mut first = connect(addr).await;
    first
        .write_all(
            b"GET /order/s1/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1\r\n\r\n",
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut buf = String::new();
    read_until(&mut first, &mut buf, "event: snapshot", WAIT).await;
    let busy = get(&app, "/order/s1/stream").await;
    assert_eq!(
        busy.json(),
        json!({"type": "about:blank", "title": "Too many connections for this instance", "status": 429})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_sends_events_and_receives_transitions() {
    let reg = registry(|b| b.max_body_bytes(256));
    let addr = serve(app(&reg)).await;
    let mut ws = connect(addr).await;
    assert!(ws_handshake(&mut ws, "/order/w1/ws", &[]).await.contains("101"));
    let snapshot = ws_read_json(&mut ws).await;
    assert_eq!(
        (snapshot["kind"].clone(), snapshot["state"].clone()),
        (json!("snapshot"), json!("pending"))
    );

    ws_send_text(&mut ws, r#"{"type":"PAY","payload":{"amount":2}}"#).await;
    let receipt = ws_read_json(&mut ws).await;
    assert_eq!(
        (receipt["kind"].clone(), receipt["state"].clone()),
        (json!("receipt"), json!("paid"))
    );
    let pushed = ws_read_json(&mut ws).await;
    assert_eq!((pushed["kind"].clone(), pushed["seq"].clone()), (json!("transition"), json!(1)));

    ws_send_text(&mut ws, "not json").await;
    assert_eq!(
        ws_read_json(&mut ws).await,
        json!({"kind": "error", "status": 400, "title": "Malformed JSON"})
    );
    ws_send_text(&mut ws, r#"{"type":5}"#).await;
    assert_eq!(
        ws_read_json(&mut ws).await,
        json!({"kind": "error", "status": 422, "title": "Bad message"})
    );
    ws_send_text(&mut ws, r#"{"type":"NOTE","payload":{"priority":1}}"#).await;
    let refused = ws_read_json(&mut ws).await;
    assert_eq!(
        (refused["kind"].clone(), refused["title"].clone()),
        (json!("error"), json!("Reserved key in payload"))
    );

    ws_send_text(&mut ws, &"x".repeat(300)).await;
    assert_eq!(ws_read_close(&mut ws).await, WS_MESSAGE_TOO_BIG);
    eventually(WAIT, || reg.connections(None) == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_refuses_cross_origin_with_1008() {
    let addr = serve(app(&registry(|b| b))).await;
    let mut ws = connect(addr).await;
    let status = ws_handshake(&mut ws, "/order/w1/ws", &[("Origin", "http://evil.example")]).await;
    assert!(status.contains("101"), "{status}");
    assert_eq!(ws_read_close(&mut ws).await, WS_POLICY_VIOLATION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_streams_and_answers_503() {
    let reg = registry(|b| b);
    reg.start().unwrap_or_else(|e| panic!("{e}"));
    let app = app(&reg).merge(reg.probes_router());
    let addr = serve(app.clone()).await;
    let mut stream = connect(addr).await;
    stream
        .write_all(b"GET /order/d1/stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut buf = String::new();
    read_until(&mut stream, &mut buf, "event: snapshot", WAIT).await;
    assert_eq!(get(&app, "/_xsm/ready").await.status, 200);

    reg.shutdown().await;
    read_until(&mut stream, &mut buf, "0\r\n\r\n", WAIT).await;
    let refused = get(&app, "/order/d1/stream").await;
    assert_eq!(
        (refused.status.as_u16(), refused.json()["title"].clone()),
        (503, json!("Shutting down"))
    );
    let send = post(&app, "/order/d1/events/NOTE", "").await;
    assert_eq!(
        send.json(),
        json!({"type": "about:blank", "title": "Shutting down", "status": 503, "error": "ShuttingDownError"})
    );
    assert_eq!(get(&app, "/_xsm/ready").await.json()["title"], "Not Ready");
    assert_eq!(get(&app, "/order/d1").await.status, 200, "reads are still served while draining");
}
