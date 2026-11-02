//! `POST /{id}/send` and `POST /{id}/events/{EVENT}`.
//!
//! CSRF (X0.7): both refuse a cross-origin `Origin` (403) and require
//! `Content-Type: application/json` even for an empty body (415), so a plain HTML form or a
//! "simple" cross-site `fetch` cannot drive an instance.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, post};
use serde_json::{Map, Value};

use super::{GATED_TITLE, RouteCtx, is_declared};
use crate::body::{ValidationIssue, bounded_json, idempotency_key_from, validation_problem};
use crate::problem::problem;
use crate::registry::SendRequest;

/// Validate a `/send` body: `{"type": <declared event>, "payload": {...}}`, nothing else
/// (the Python fallback model, `extra="forbid"`).
fn validate_send_body(
    body: Option<Value>,
    events: &[String],
) -> Result<(String, Map<String, Value>), Vec<ValidationIssue>> {
    let Some(body) = body else {
        return Err(vec![ValidationIssue::new(&["body"], "missing")]);
    };
    let Value::Object(mut map) = body else {
        return Err(vec![ValidationIssue::new(&["body"], "model_attributes_type")]);
    };
    let mut issues = Vec::new();
    let ty = match map.remove("type") {
        None => {
            issues.push(ValidationIssue::new(&["body", "type"], "missing"));
            None
        }
        Some(Value::String(t)) if is_declared(events, &t) => Some(t),
        Some(_) => {
            issues.push(ValidationIssue::new(&["body", "type"], "literal_error"));
            None
        }
    };
    let payload = match map.remove("payload") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p,
        Some(_) => {
            issues.push(ValidationIssue::new(&["body", "payload"], "dict_type"));
            Map::new()
        }
    };
    for extra in map.keys() {
        issues.push(ValidationIssue::new(&["body", extra.as_str()], "extra_forbidden"));
    }
    match ty {
        Some(ty) if issues.is_empty() => Ok((ty, payload)),
        _ => Err(issues),
    }
}

async fn dispatch(
    ctx: &RouteCtx,
    parts: &Parts,
    key: String,
    event: String,
    payload: Map<String, Value>,
) -> Response {
    let idempotency_key = match idempotency_key_from(&parts.headers) {
        Ok(k) => k,
        Err(error) => return error.into_response(),
    };
    let request = SendRequest {
        machine: ctx.name.clone(),
        key,
        event,
        payload,
        principal: ctx.registry.principal_of(parts),
        idempotency_key,
        create_if_missing: ctx.create_if_missing,
    };
    ctx.registry.send_event(request).await
}

/// The cross-origin refusal of a state-changing route (X0.7, CSRF).
fn refuse_cross_origin(ctx: &RouteCtx, parts: &Parts) -> Option<Response> {
    (!ctx.registry.origin_allowed(&parts.headers))
        .then(|| problem(403, "Origin not allowed").into_response())
}

/// `POST /{id}/send`: send the body's event; answers the receipt.
pub(super) async fn send_any(
    State(ctx): State<Arc<RouteCtx>>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if let Some(refused) = refuse_cross_origin(&ctx, &parts) {
        return refused;
    }
    let settings = &ctx.registry.settings;
    let body =
        match bounded_json(&parts.headers, body, settings.max_body_bytes, settings.body_timeout)
            .await
        {
            Ok(body) => body,
            Err(problem) => return problem.into_response(),
        };
    let (event, payload) = match validate_send_body(body, &ctx.events) {
        Ok(ok) => ok,
        Err(issues) => return validation_problem(&issues).into_response(),
    };
    if ctx.gated.contains_key(&event) {
        return problem(403, GATED_TITLE).into_response();
    }
    dispatch(&ctx, &parts, key, event, payload).await
}

/// `POST /{id}/events/{event}`: run the event's gate, then send it with the optional object
/// body as payload.
async fn send_one(ctx: Arc<RouteCtx>, key: String, request: Request, event: String) -> Response {
    let (parts, body) = request.into_parts();
    if let Some(refused) = refuse_cross_origin(&ctx, &parts) {
        return refused;
    }
    if let Some(gate) = ctx.gated.get(&event)
        && let Err(problem) = gate.check(&parts)
    {
        return problem.into_response();
    }
    let settings = &ctx.registry.settings;
    let payload =
        match bounded_json(&parts.headers, body, settings.max_body_bytes, settings.body_timeout)
            .await
        {
            Ok(None | Some(Value::Null)) => Map::new(),
            Ok(Some(Value::Object(map))) => map,
            Ok(Some(_)) => {
                return validation_problem(&[ValidationIssue::new(&["body"], "dict_type")])
                    .into_response();
            }
            Err(problem) => return problem.into_response(),
        };
    dispatch(&ctx, &parts, key, event, payload).await
}

/// The method router of one `/events/{event}` route.
pub(super) fn event_route(event: String) -> MethodRouter<Arc<RouteCtx>> {
    post(move |State(ctx): State<Arc<RouteCtx>>, Path(key): Path<String>, request: Request| {
        let event = event.clone();
        async move { send_one(ctx, key, request, event).await }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_body_validation() {
        let events = vec!["GO".to_owned()];
        let ok = validate_send_body(Some(serde_json::json!({"type": "GO"})), &events);
        assert!(matches!(ok, Ok((ref t, ref p)) if t == "GO" && p.is_empty()));
        let bad = validate_send_body(Some(serde_json::json!({"type": "NOPE", "x": 1})), &events);
        let kinds: Vec<&str> = bad.err().unwrap_or_default().iter().map(|i| i.kind).collect();
        assert_eq!(kinds, ["literal_error", "extra_forbidden"]);
        assert!(validate_send_body(None, &events).is_err());
    }
}
