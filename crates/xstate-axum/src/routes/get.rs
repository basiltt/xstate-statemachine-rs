//! `GET /{id}`: the instance's state.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::RouteCtx;

/// `GET /{id}`: the state body plus `machine_version` (served from the snapshot, no lock).
pub(super) async fn get_state(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if let Err(error) = ctx.guard_read(&parts, &key).await {
        return error.into_response();
    }
    match ctx.registry.peek(&ctx.name, &key).await {
        Ok(mut body) => {
            if let Value::Object(map) = &mut body {
                let version = ctx.reg.machine.version().map(str::to_owned);
                map.insert("machine_version".into(), Value::from(version));
            }
            Json(body).into_response()
        }
        Err(error) => error.into_response(),
    }
}
