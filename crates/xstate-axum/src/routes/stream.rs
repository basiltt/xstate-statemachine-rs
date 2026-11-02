//! `GET /{id}/stream`: Server-Sent Events (Python `transition_stream`).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use xstate_core::pyjson::dumps_compact;

use super::RouteCtx;
use crate::fanout::{FanoutItem, Subscription};
use crate::problem::problem;
use crate::registry::{ConnectionSlot, StatechartRegistry};

const HEARTBEAT: &[u8] = b": heartbeat\n\n";

/// One SSE frame: `id: <seq>` (when given), `event: <event>`, `data: <compact JSON>`.
fn sse_frame(event: &str, data: &Value, seq: Option<u64>) -> Bytes {
    let id = seq.map(|s| format!("id: {s}\n")).unwrap_or_default();
    Bytes::from(format!("{id}event: {event}\ndata: {}\n\n", dumps_compact(data)))
}

struct SseState {
    pending: Option<Bytes>,
    subscription: Subscription,
    heartbeat: Duration,
    // Released (with the subscription) when the client goes away and the body is dropped.
    _slot: ConnectionSlot,
}

async fn next_frame(mut st: SseState) -> Option<(Result<Bytes, Infallible>, SseState)> {
    if let Some(first) = st.pending.take() {
        return Some((Ok(first), st));
    }
    match tokio::time::timeout(st.heartbeat, st.subscription.recv()).await {
        Err(_) => Some((Ok(Bytes::from_static(HEARTBEAT)), st)),
        Ok(FanoutItem::Closed) => None,
        Ok(FanoutItem::Transition { seq, body }) => {
            Some((Ok(sse_frame("transition", &body, Some(seq))), st))
        }
    }
}

/// `text/event-stream` of one instance: `event: snapshot` on connect, then
/// `event: transition` once per changed receipt committed in **this** process (`id:` = the
/// per-instance sequence), and a `: heartbeat` comment every heartbeat interval.
///
/// Refusals are problems: 403 (origin / authorizer), 503 (draining), 429
/// (`max_connections_per_key`). The subscription and the connection slot are released when
/// the client disconnects (the response body is dropped).
pub async fn transition_stream(
    registry: &Arc<StatechartRegistry>,
    name: &str,
    key: &str,
    headers: &HeaderMap,
    principal: Option<&str>,
) -> Response {
    if !registry.origin_allowed(headers) {
        return problem(403, "Origin not allowed").into_response();
    }
    if let Err(error) = registry.authorize(principal, name, key, None) {
        return error.into_response();
    }
    let snapshot = match registry.peek(name, key).await {
        Ok(snapshot) => snapshot,
        Err(error) => return error.into_response(),
    };
    if registry.is_draining() {
        return problem(503, "Shutting down").into_response();
    }
    let Some(slot) = registry.try_open_connection(name, key) else {
        return problem(429, "Too many connections for this instance").into_response();
    };
    let subscription = registry.subscribers().subscribe(name, key);
    let seq0 = registry.subscribers().seq(name, key);
    let state = SseState {
        pending: Some(sse_frame("snapshot", &snapshot, Some(seq0))),
        subscription,
        heartbeat: registry.settings.heartbeat,
        _slot: slot,
    };
    let body = Body::from_stream(futures_util::stream::unfold(state, next_frame));
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// `GET /{id}/stream`.
pub(super) async fn stream(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if let Err(error) = ctx.guard_read(&parts, &key).await {
        return error.into_response();
    }
    let principal = ctx.registry.principal_of(&parts);
    transition_stream(&ctx.registry, &ctx.name, &key, &parts.headers, principal.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_matches_python() {
        let frame = sse_frame("transition", &serde_json::json!({"a": "é"}), Some(3));
        assert_eq!(&frame[..], b"id: 3\nevent: transition\ndata: {\"a\":\"\\u00e9\"}\n\n");
    }
}
