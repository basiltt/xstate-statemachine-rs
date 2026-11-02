//! `GET /{id}/diagram.mmd`: the chart as a Mermaid diagram.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use super::RouteCtx;

/// `GET /{id}/diagram.mmd`: `text/plain` Mermaid `stateDiagram-v2` (authorized as a read).
pub(super) async fn diagram(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let principal = ctx.registry.principal_of(&parts);
    if let Err(error) = ctx.registry.authorize(principal.as_deref(), &ctx.name, &key, None) {
        return error.into_response();
    }
    let mut response = ctx.reg.machine.to_mermaid().into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}
