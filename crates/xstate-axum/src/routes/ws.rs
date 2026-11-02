//! `GET /{id}/ws`: the WebSocket endpoint (Python `websocket_endpoint`).
//!
//! Protocol: on connect the server sends `{"kind": "snapshot", ...}`; the client sends
//! `{"type": "EVENT", "payload": {...}}` and gets `{"kind": "receipt", ...}` or
//! `{"kind": "error", "status", "title", ...}`; committed transitions from any client arrive as
//! `{"kind": "transition", "seq": n, ...}`; `{"kind": "ping"}` is sent when idle for a
//! heartbeat interval.
//!
//! A frame obeys the same rules as `POST /{id}/send`: an event the machine does not declare is
//! a 422 validation error frame, and an event with a per-event gate is refused with
//! `{"kind": "error", "status": 403, "title": "Use this event's dedicated route"}` (the gate
//! can only run on that event's HTTP route).
//!
//! Close codes: 1001 shutting down, 1008 origin / authorizer refused (or unknown key with
//! `create_if_missing = false`), 1009 frame over `max_body_bytes`, 1011 the snapshot failed to
//! load or the client did not accept a frame within two heartbeat intervals, 1013 too many
//! connections or cut for lagging. A malformed frame answers
//! `{"kind": "error", "status": 400 | 422}` and the session stays open.
//!
//! Refusals happen after the upgrade (accept, then close with the code) so the client always
//! sees the code; Starlette refuses before `accept` with an HTTP 403 instead.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::future::{AbortHandle, Abortable};
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use xstate_persistence::StoreError;

use super::{GATED_TITLE, RouteCtx, is_declared};
use crate::body::{ValidationIssue, validation_problem};
use crate::fanout::{FanoutItem, Subscription};
use crate::problem::ApiError;
use crate::registry::{ConnectionRefused, ConnectionSlot, SendRequest};

/// Close code: the server is going away (shutdown).
pub const WS_GOING_AWAY: u16 = 1001;
/// Close code: origin or authorizer refused.
pub const WS_POLICY_VIOLATION: u16 = 1008;
/// Close code: frame over `max_body_bytes`.
pub const WS_MESSAGE_TOO_BIG: u16 = 1009;
/// Close code: the snapshot could not be loaded, or the client stopped reading.
pub const WS_INTERNAL_ERROR: u16 = 1011;
/// Close code: too many connections, or cut off for lagging.
pub const WS_TRY_AGAIN_LATER: u16 = 1013;

/// Transport limit above `max_body_bytes`, so an oversized frame reaches the 1009 check
/// instead of failing inside the protocol layer.
const FRAME_SLACK: usize = 64 * 1024;

pub(crate) fn with_kind(kind: &str, body: Value) -> Value {
    let mut out = Map::new();
    out.insert("kind".into(), Value::from(kind));
    if let Value::Object(map) = body {
        out.extend(map);
    }
    Value::Object(out)
}

/// Send a close frame, giving up after `limit` (a stuck peer is simply dropped).
pub(crate) async fn close<S: Sink<Message> + Unpin>(sink: &mut S, code: u16, limit: Duration) {
    let frame = CloseFrame { code, reason: Utf8Bytes::from_static("") };
    let _ = tokio::time::timeout(limit, sink.send(Message::Close(Some(frame)))).await;
}

/// Send one JSON text frame within `limit`. `false` when the peer is gone, or did not accept
/// the frame in time (it is then closed with 1011, itself bounded by `limit`).
pub(crate) async fn send_json<S: Sink<Message> + Unpin>(
    sink: &mut S,
    value: &Value,
    limit: Duration,
) -> bool {
    let send = sink.send(Message::text(value.to_string()));
    if let Ok(sent) = tokio::time::timeout(limit, send).await {
        return sent.is_ok();
    }
    tracing::warn!(target: "xstate::axum", ?limit, "websocket client stopped reading; closing with 1011");
    close(sink, WS_INTERNAL_ERROR, limit).await;
    false
}

/// `GET /{id}/ws`.
pub(super) async fn websocket(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    upgrade: WebSocketUpgrade,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let principal = ctx.registry.principal_of(&parts);
    let limit = ctx.registry.max_body_bytes().saturating_add(FRAME_SLACK);
    upgrade.max_message_size(limit).max_frame_size(limit).on_upgrade(move |socket| async move {
        // Abortable so that `shutdown()` can end a session that does not finish on its own.
        let (handle, registration) = AbortHandle::new_pair();
        let session = session(ctx, key, principal, parts.headers, socket, handle);
        let _ = Abortable::new(session, registration).await;
    })
}

struct Opened {
    snapshot: Value,
    subscription: Subscription,
    _slot: ConnectionSlot,
}

/// Origin, authorizer, existence, slot, subscription and snapshot; `Err(close code)`.
///
/// Authorization runs before the existence check (as `guard_read` does over HTTP), so a
/// refused caller cannot probe which keys exist.
///
/// Ordering rule: subscribe **before** reading the snapshot. A transition committed in
/// between is then both in the snapshot and in the queue (a harmless duplicate); the other
/// order would lose it.
async fn connect(
    ctx: &RouteCtx,
    key: &str,
    principal: Option<&str>,
    headers: &HeaderMap,
    handle: AbortHandle,
) -> Result<Opened, u16> {
    if !ctx.registry.origin_allowed(headers) {
        return Err(WS_POLICY_VIOLATION);
    }
    match ctx.registry.authorize(principal, &ctx.name, key, None) {
        Ok(()) => {}
        Err(ApiError::Forbidden) => return Err(WS_POLICY_VIOLATION),
        Err(_) => return Err(WS_INTERNAL_ERROR),
    }
    match ctx.require_existing(key).await {
        Ok(()) => {}
        Err(ApiError::Store(StoreError::NotFound(_))) => return Err(WS_POLICY_VIOLATION),
        Err(_) => return Err(WS_INTERNAL_ERROR),
    }
    let slot =
        ctx.registry.try_open_connection(&ctx.name, key).map_err(|refused| match refused {
            ConnectionRefused::Draining => WS_GOING_AWAY,
            ConnectionRefused::PerKey | ConnectionRefused::Global => WS_TRY_AGAIN_LATER,
        })?;
    slot.attach(handle);
    let subscription = ctx.registry.subscribers().subscribe(&ctx.name, key);
    let snapshot = ctx.registry.peek(&ctx.name, key).await.map_err(|error| {
        tracing::error!(target: "xstate::axum", %key, %error, "websocket connect failed");
        WS_INTERNAL_ERROR
    })?;
    Ok(Opened { snapshot, subscription, _slot: slot })
}

/// Longest a client may take to accept one frame: two heartbeat intervals.
fn send_limit(ctx: &RouteCtx) -> Duration {
    ctx.registry.settings.heartbeat.saturating_mul(2)
}

async fn session(
    ctx: Arc<RouteCtx>,
    key: String,
    principal: Option<String>,
    headers: HeaderMap,
    mut socket: WebSocket,
    handle: AbortHandle,
) {
    let limit = send_limit(&ctx);
    let opened = match connect(&ctx, &key, principal.as_deref(), &headers, handle).await {
        Ok(opened) => opened,
        Err(code) => return close(&mut socket, code, limit).await,
    };
    let Opened { snapshot, subscription, _slot } = opened;
    if !send_json(&mut socket, &with_kind("snapshot", snapshot), limit).await {
        return;
    }
    pump(&ctx, &key, principal, socket, subscription).await;
}

/// What to do after one inbound frame.
enum Reply {
    Frame(Value),
    Close(u16),
}

/// The `/send` admission rules for a frame's event: declared (422 otherwise) and not gated
/// (403 otherwise).
fn refuse_event(ctx: &RouteCtx, event: &str) -> Option<Value> {
    if !is_declared(&ctx.events, event) {
        let issue = ValidationIssue::new(&["body", "type"], "literal_error");
        return Some(with_kind("error", validation_problem(&[issue]).body()));
    }
    if ctx.gated.contains_key(event) {
        return Some(json!({"kind": "error", "status": 403, "title": GATED_TITLE}));
    }
    None
}

/// Decode one frame and send it as an event (Python `_decode_frame` / `on_receive`).
async fn handle_frame(ctx: &RouteCtx, key: &str, principal: Option<&String>, raw: &[u8]) -> Reply {
    let Ok(data) = serde_json::from_slice::<Value>(raw) else {
        return Reply::Frame(json!({"kind": "error", "status": 400, "title": "Malformed JSON"}));
    };
    let event = data.get("type").and_then(Value::as_str);
    let payload = match data.get("payload") {
        None => Some(Map::new()),
        Some(Value::Object(map)) => Some(map.clone()),
        Some(_) => None,
    };
    let (Some(event), Some(payload)) = (event, payload) else {
        return Reply::Frame(json!({"kind": "error", "status": 422, "title": "Bad message"}));
    };
    if let Some(refusal) = refuse_event(ctx, event) {
        return Reply::Frame(refusal);
    }
    let request = SendRequest {
        machine: ctx.name.clone(),
        key: key.to_owned(),
        event: event.to_owned(),
        payload,
        principal: principal.cloned(),
        idempotency_key: None,
        create_if_missing: ctx.create_if_missing,
    };
    match ctx.registry.send(request).await {
        Ok(outcome) => Reply::Frame(with_kind("receipt", outcome.body)),
        Err(ApiError::Forbidden) => Reply::Close(WS_POLICY_VIOLATION),
        Err(error) => Reply::Frame(with_kind("error", error.to_problem().body())),
    }
}

/// One inbound WebSocket message, classified.
enum Inbound {
    Data(Bytes),
    Skip,
    End,
}

fn classify(message: Message) -> Inbound {
    match message {
        Message::Text(text) => Inbound::Data(Bytes::copy_from_slice(text.as_str().as_bytes())),
        Message::Binary(bytes) => Inbound::Data(bytes),
        Message::Close(_) => Inbound::End,
        Message::Ping(_) | Message::Pong(_) => Inbound::Skip,
    }
}

/// The frame for one fan-out item (`None`: idle for a heartbeat, so a ping), or the close
/// code ending the session.
fn outbound(ctx: &RouteCtx, item: Option<FanoutItem>) -> Result<Value, u16> {
    match item {
        None => Ok(json!({"kind": "ping"})),
        Some(FanoutItem::Closed) => {
            Err(if ctx.registry.is_draining() { WS_GOING_AWAY } else { WS_TRY_AGAIN_LATER })
        }
        Some(FanoutItem::Transition { seq, body }) => {
            let mut out = with_kind("transition", json!({"seq": seq}));
            if let (Value::Object(o), Value::Object(b)) = (&mut out, body.as_ref()) {
                o.extend(b.clone());
            }
            Ok(out)
        }
    }
}

/// Forward client frames as events and committed transitions (and pings) to the client
/// until either side goes away. Every send is bounded by two heartbeat intervals.
async fn pump(
    ctx: &RouteCtx,
    key: &str,
    principal: Option<String>,
    socket: WebSocket,
    mut subscription: Subscription,
) {
    let (mut tx, mut rx) = socket.split();
    let heartbeat = ctx.registry.settings.heartbeat;
    let limit = send_limit(ctx);
    let max = ctx.registry.max_body_bytes();
    loop {
        let frame = tokio::select! {
            inbound = rx.next() => {
                let Some(Ok(message)) = inbound else { return };
                let raw = match classify(message) {
                    Inbound::Data(raw) => raw,
                    Inbound::Skip => continue,
                    Inbound::End => return,
                };
                if raw.len() > max {
                    return close(&mut tx, WS_MESSAGE_TOO_BIG, limit).await;
                }
                match handle_frame(ctx, key, principal.as_ref(), &raw).await {
                    Reply::Frame(frame) => frame,
                    Reply::Close(code) => return close(&mut tx, code, limit).await,
                }
            }
            item = tokio::time::timeout(heartbeat, subscription.recv()) => {
                match outbound(ctx, item.ok()) {
                    Ok(frame) => frame,
                    Err(code) => return close(&mut tx, code, limit).await,
                }
            }
        };
        if !send_json(&mut tx, &frame, limit).await {
            return;
        }
    }
}
