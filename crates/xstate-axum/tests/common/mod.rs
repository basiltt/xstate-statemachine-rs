//! Shared fixtures for the integration tests.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use xstate_axum::{RegisterOptions, RegistryBuilder, RouterOptions, StatechartRegistry, router};
use xstate_core::{Machine, MachineLogic};
use xstate_persistence::{MemoryStore, StateStore};

pub(crate) const ORDER: &str = r#"{"id":"order","initial":"pending","states":{
  "pending":{"on":{"PAY":{"target":"paid","guard":"amountOk"},"CANCEL":"cancelled","NOTE":{"actions":"note"}}},
  "paid":{"on":{"SHIP":"shipped"}},
  "shipped":{"type":"final"},
  "cancelled":{"type":"final"}}}"#;

pub(crate) fn order_machine() -> Machine {
    Machine::from_json(ORDER).unwrap_or_else(|e| panic!("{e}"))
}

pub(crate) fn order_logic() -> MachineLogic<Value> {
    MachineLogic::dynamic()
        .guard("amountOk", |_, event, _| {
            Ok(event.payload.get("amount").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
        })
        .action("note", |_| Ok(()))
}

pub(crate) fn store() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new())
}

/// A registry over `store` with `order` registered (default options), customised by `f`.
pub(crate) fn registry_over(
    store: Arc<dyn StateStore>,
    f: impl FnOnce(RegistryBuilder) -> RegistryBuilder,
) -> Arc<StatechartRegistry> {
    f(StatechartRegistry::builder(store))
        .register("order", order_machine(), order_logic(), RegisterOptions::default())
        .and_then(RegistryBuilder::build)
        .unwrap_or_else(|e| panic!("{e}"))
}

pub(crate) fn registry(
    f: impl FnOnce(RegistryBuilder) -> RegistryBuilder,
) -> Arc<StatechartRegistry> {
    registry_over(store(), f)
}

pub(crate) fn app(registry: &Arc<StatechartRegistry>) -> Router {
    router(registry, "order", RouterOptions::default()).unwrap_or_else(|e| panic!("{e}"))
}

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl Reply {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(crate) fn content_type(&self) -> &str {
        self.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("")
    }
}

/// One request through the router (`tower::ServiceExt::oneshot`). A body is sent as JSON
/// unless a `content-type` header is given.
pub(crate) async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        builder = builder.header("host", "127.0.0.1");
    }
    let has_ctype = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if body.is_some() && !has_ctype {
        builder = builder.header("content-type", "application/json");
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_owned()));
    let request = builder.body(body).unwrap_or_else(|e| panic!("{e}"));
    let response = app.clone().oneshot(request).await.unwrap_or_else(|e| panic!("{e}"));
    let (parts, body) = response.into_parts();
    let body = body.collect().await.unwrap_or_else(|e| panic!("{e}")).to_bytes();
    Reply { status: parts.status, headers: parts.headers, body }
}

pub(crate) async fn get(app: &Router, uri: &str) -> Reply {
    call(app, "GET", uri, &[], None).await
}

pub(crate) async fn post(app: &Router, uri: &str, body: &str) -> Reply {
    call(app, "POST", uri, &[], Some(body)).await
}

/// Serve `app` on 127.0.0.1:0; returns the address.
pub(crate) async fn serve(app: Router) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap_or_else(|e| panic!("{e}"));
    let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

pub(crate) async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap_or_else(|e| panic!("{e}"))
}

/// Read from `stream` into `buf` until `needle` appears (or panic after `timeout`).
pub(crate) async fn read_until(
    stream: &mut TcpStream,
    buf: &mut String,
    needle: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !buf.contains(needle) {
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got {buf:?}"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "connection closed waiting for {needle:?}; got {buf:?}");
        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
}

/// Poll `check` every 10 ms until it holds (or panic after `timeout`).
pub(crate) async fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "condition not reached in {timeout:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ------------------------------------------------------------------ minimal WebSocket client

/// Send an HTTP upgrade request and read the response head; returns the status line.
pub(crate) async fn ws_handshake(
    stream: &mut TcpStream,
    path: &str,
    extra: &[(&str, &str)],
) -> String {
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (k, v) in extra {
        let _ = write!(request, "{k}: {v}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap_or_else(|e| panic!("{e}"));
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        let n = stream.read(&mut byte).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed during handshake");
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned()
}

/// A server frame: `(opcode, payload)`.
pub(crate) async fn ws_read(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let read = async {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).await?;
        let len = match head[1] & 0x7f {
            126 => {
                let mut ext = [0u8; 2];
                stream.read_exact(&mut ext).await?;
                u64::from(u16::from_be_bytes(ext))
            }
            127 => {
                let mut ext = [0u8; 8];
                stream.read_exact(&mut ext).await?;
                u64::from_be_bytes(ext)
            }
            n => u64::from(n),
        };
        let mut payload = vec![0u8; usize::try_from(len).unwrap_or(0)];
        stream.read_exact(&mut payload).await?;
        Ok::<_, std::io::Error>((head[0] & 0x0f, payload))
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .unwrap_or_else(|_| panic!("timed out reading a frame"))
        .unwrap_or_else(|e| panic!("{e}"))
}

/// The next text frame as JSON (skipping pings).
pub(crate) async fn ws_read_json(stream: &mut TcpStream) -> Value {
    loop {
        let (opcode, payload) = ws_read(stream).await;
        assert_eq!(opcode, 1, "expected a text frame, got opcode {opcode}: {payload:?}");
        let value: Value = serde_json::from_slice(&payload).unwrap_or_else(|e| panic!("{e}"));
        if value["kind"] != "ping" {
            return value;
        }
    }
}

/// The close code of the next frame, which must be a close frame.
pub(crate) async fn ws_read_close(stream: &mut TcpStream) -> u16 {
    let (opcode, payload) = ws_read(stream).await;
    assert_eq!(opcode, 8, "expected a close frame: {}", String::from_utf8_lossy(&payload));
    u16::from_be_bytes([payload[0], payload[1]])
}

/// Send a masked client text frame.
pub(crate) async fn ws_send_text(stream: &mut TcpStream, text: &str) {
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    let payload = text.as_bytes();
    let mut frame = vec![0x81u8];
    match payload.len() {
        n if n < 126 => frame.push(0x80 | u8::try_from(n).unwrap_or(0)),
        n => {
            frame.push(0x80 | 0x7e);
            frame.extend_from_slice(&u16::try_from(n).unwrap_or(u16::MAX).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).await.unwrap_or_else(|e| panic!("{e}"));
}
