//! `GET /{id}/ws`: the WebSocket endpoint (Python `websocket_endpoint`).
//!
//! Protocol: on connect the server sends `{"kind": "snapshot", ...}`; the client sends
//! `{"type": "EVENT", "payload": {...}}` and gets `{"kind": "receipt", ...}` or
//! `{"kind": "error", "status", "title", ...}`; committed transitions from any client arrive as
//! `{"kind": "transition", "seq": n, ...}`; `{"kind": "ping"}` is sent when idle for a
//! heartbeat interval.
//!
//! Close codes: 1001 shutting down, 1008 origin / authorizer refused (or unknown key with
//! `create_if_missing = false`), 1009 frame over `max_body_bytes`, 1011 the snapshot failed to
//! load, 1013 too many connections or cut for lagging. A malformed frame answers
//! `{"kind": "error", "status": 400 | 422}` and the session stays open.
//!
//! Refusals happen after the upgrade (accept, then close with the code) so the client always
//! sees the code; Starlette refuses before `accept` with an HTTP 403 instead.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use xstate_persistence::StoreError;

use super::RouteCtx;
use crate::fanout::{FanoutItem, Subscription};
use crate::problem::ApiError;
use crate::registry::{ConnectionSlot, SendRequest};

/// Close code: the server is going away (shutdown).
pub const WS_GOING_AWAY: u16 = 1001;
/// Close code: origin or authorizer refused.
pub const WS_POLICY_VIOLATION: u16 = 1008;
/// Close code: frame over `max_body_bytes`.
pub const WS_MESSAGE_TOO_BIG: u16 = 1009;
/// Close code: the snapshot could not be loaded.
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

pub(crate) async fn close<S: Sink<Message> + Unpin>(sink: &mut S, code: u16) {
    let frame = CloseFrame { code, reason: Utf8Bytes::from_static("") };
    let _ = sink.send(Message::Close(Some(frame))).await;
}

pub(crate) async fn send_json<S: Sink<Message> + Unpin>(sink: &mut S, value: &Value) -> bool {
    sink.send(Message::text(value.to_string())).await.is_ok()
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
    upgrade
        .max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| session(ctx, key, principal, parts.headers, socket))
}

struct Opened {
    snapshot: Value,
    subscription: Subscription,
    _slot: ConnectionSlot,
}

/// Origin, existence, authorizer, snapshot, drain and slot checks; `Err(close code)`.
async fn connect(
    ctx: &RouteCtx,
    key: &str,
    principal: Option<&str>,
    headers: &HeaderMap,
) -> Result<Opened, u16> {
    if !ctx.registry.origin_allowed(headers) {
        return Err(WS_POLICY_VIOLATION);
    }
    match ctx.require_existing(key).await {
        Ok(()) => {}
        Err(ApiError::Store(StoreError::NotFound(_))) => return Err(WS_POLICY_VIOLATION),
        Err(_) => return Err(WS_INTERNAL_ERROR),
    }
    match ctx.registry.authorize(principal, &ctx.name, key, None) {
        Ok(()) => {}
        Err(ApiError::Forbidden) => return Err(WS_POLICY_VIOLATION),
        Err(_) => return Err(WS_INTERNAL_ERROR),
    }
    let snapshot = ctx.registry.peek(&ctx.name, key).await.map_err(|error| {
        tracing::error!(target: "xstate::axum", %key, %error, "websocket connect failed");
        WS_INTERNAL_ERROR
    })?;
    if ctx.registry.is_draining() {
        return Err(WS_GOING_AWAY);
    }
    let slot = ctx.registry.try_open_connection(&ctx.name, key).ok_or(WS_TRY_AGAIN_LATER)?;
    let subscription = ctx.registry.subscribers().subscribe(&ctx.name, key);
    Ok(Opened { snapshot, subscription, _slot: slot })
}

async fn session(
    ctx: Arc<RouteCtx>,
    key: String,
    principal: Option<String>,
    headers: HeaderMap,
    mut socket: WebSocket,
) {
    let opened = match connect(&ctx, &key, principal.as_deref(), &headers).await {
        Ok(opened) => opened,
        Err(code) => return close(&mut socket, code).await,
    };
    let Opened { snapshot, subscription, _slot } = opened;
    if !send_json(&mut socket, &with_kind("snapshot", snapshot)).await {
        return;
    }
    pump(&ctx, &key, principal, socket, subscription).await;
}

/// What to do after one inbound frame.
enum Reply {
    Frame(Value),
    Close(u16),
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

/// Forward client frames as events and committed transitions (and pings) to the client
/// until either side goes away.
async fn pump(
    ctx: &RouteCtx,
    key: &str,
    principal: Option<String>,
    socket: WebSocket,
    mut subscription: Subscription,
) {
    let (mut tx, mut rx) = socket.split();
    let heartbeat = ctx.registry.settings.heartbeat;
    let max = ctx.registry.max_body_bytes();
    loop {
        tokio::select! {
            inbound = rx.next() => {
                let Some(Ok(message)) = inbound else { return };
                let raw = match classify(message) {
                    Inbound::Data(raw) => raw,
                    Inbound::Skip => continue,
                    Inbound::End => return,
                };
                if raw.len() > max {
                    return close(&mut tx, WS_MESSAGE_TOO_BIG).await;
                }
                match handle_frame(ctx, key, principal.as_ref(), &raw).await {
                    Reply::Frame(frame) => {
                        if !send_json(&mut tx, &frame).await {
                            return;
                        }
                    }
                    Reply::Close(code) => return close(&mut tx, code).await,
                }
            }
            item = tokio::time::timeout(heartbeat, subscription.recv()) => {
                let frame = match item {
                    Err(_) => json!({"kind": "ping"}),
                    Ok(FanoutItem::Closed) => {
                        let code = if ctx.registry.is_draining() { WS_GOING_AWAY } else { WS_TRY_AGAIN_LATER };
                        return close(&mut tx, code).await;
                    }
                    Ok(FanoutItem::Transition { seq, body }) => {
                        let mut out = with_kind("transition", json!({"seq": seq}));
                        if let (Value::Object(o), Value::Object(b)) = (&mut out, body.as_ref()) {
                            o.extend(b.clone());
                        }
                        out
                    }
                };
                if !send_json(&mut tx, &frame).await {
                    return;
                }
            }
        }
    }
}
