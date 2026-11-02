//! `GET /{id}/events`: events accepted now, and every declared event.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::RouteCtx;

/// `GET /{id}/events`: `{"available": [...], "declared": [{"type", "schema"}]}`. The Rust
/// engine has no per-event payload schemas, so `schema` is always `null`.
pub(super) async fn list_events(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if let Err(error) = ctx.guard_read(&parts, &key).await {
        return error.into_response();
    }
    let available = match ctx.registry.peek(&ctx.name, &key).await {
        Ok(body) => body.get("available_events").cloned().unwrap_or(Value::Array(Vec::new())),
        Err(error) => return error.into_response(),
    };
    let declared: Vec<Value> =
        ctx.events.iter().map(|e| json!({"type": e, "schema": Value::Null})).collect();
    Json(json!({"available": available, "declared": declared})).into_response()
}
