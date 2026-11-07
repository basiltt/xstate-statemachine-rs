//! Request-body helpers: bounded JSON bodies, the `Idempotency-Key` header and the reserved
//! `send()` option names (Python `_http.py` `json_body` / `read_body` and the `FastAPI` router's
//! `_bounded_json`).
//!
//! X0.7: bodies must be `application/json` and bounded in size (413) and in time (408).

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use serde_json::{Map, Value};

use crate::problem::{ApiError, Problem};

/// Header carrying the client's idempotency key.
pub const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";

/// `send()` keyword options a client body may never carry (X0.7).
pub const RESERVED_SEND_KEYS: [&str; 2] = ["wait", "priority"];

/// Payload keys the adapter owns. `idempotency_key` comes only from the validated
/// `Idempotency-Key` header: a body that carried one skipped the header's length/charset check
/// and the "no inbox" refusal, so an over-long key produced a 200 whose event never ran (Python
/// review CW-04). A body `id` is ordinary event data and stays allowed.
pub const ADAPTER_OWNED_PAYLOAD_KEYS: [&str; 1] = ["idempotency_key"];

/// Default upper bound on reading one request body (battle #277-a).
pub const DEFAULT_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Most validation errors listed in one 422 problem (battle #276-a).
pub const MAX_VALIDATION_ERRORS: usize = 50;

/// The `Idempotency-Key` header, or `None`; [`ApiError::BadRequest`] when it is malformed
/// (not printable ASCII, longer than 255 characters).
///
/// # Errors
/// [`ApiError::BadRequest`] `"Invalid Idempotency-Key header"`.
pub fn idempotency_key_from(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(raw) = headers.get(IDEMPOTENCY_HEADER) else { return Ok(None) };
    if raw.is_empty() {
        return Ok(None);
    }
    let invalid = || ApiError::BadRequest("Invalid Idempotency-Key header".into());
    let value = raw.to_str().map_err(|_| invalid())?;
    xstate_persistence::validate_idempotency_key(value).map_err(|_| invalid())?;
    Ok(Some(value.to_owned()))
}

/// Refuse a payload that names a `send()` option (`wait`, `priority`) or an adapter-owned key
/// (`idempotency_key`).
///
/// # Errors
/// [`ApiError::ReservedKey`] (422).
pub fn refuse_reserved_send_keys(payload: &Map<String, Value>) -> Result<(), ApiError> {
    if RESERVED_SEND_KEYS.iter().any(|k| payload.contains_key(*k))
        || ADAPTER_OWNED_PAYLOAD_KEYS.iter().any(|k| payload.contains_key(*k))
    {
        return Err(ApiError::ReservedKey);
    }
    Ok(())
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(header::CONTENT_LENGTH)?.to_str().ok()?;
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn is_json(headers: &HeaderMap) -> bool {
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    ctype.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json")
}

/// Read a body of at most `max_body_bytes` (413) within `timeout` (408).
///
/// # Errors
/// [`ApiError::PayloadTooLarge`], [`ApiError::RequestTimeout`], or
/// [`ApiError::BadRequest`] when the client broke the connection.
pub async fn read_body(
    body: Body,
    max_body_bytes: usize,
    timeout: Duration,
) -> Result<Bytes, ApiError> {
    let read = async move {
        let mut stream = body.into_data_stream();
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ApiError::BadRequest("Bad Request".into()))?;
            if out.len() + chunk.len() > max_body_bytes {
                return Err(ApiError::PayloadTooLarge);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(out))
    };
    tokio::time::timeout(timeout, read).await.unwrap_or(Err(ApiError::RequestTimeout))
}

/// Read a bounded `application/json` **object** body (Python `json_body`). An empty body
/// (declared `Content-Length: 0` or whitespace only) is `{}`.
///
/// # Errors
/// 413 / 415 / 408 / 422 as [`ApiError`]s.
pub async fn json_body(
    headers: &HeaderMap,
    body: Body,
    max_body_bytes: usize,
    timeout: Duration,
) -> Result<Map<String, Value>, ApiError> {
    if let Some(declared) = declared_length(headers) {
        if declared > max_body_bytes as u64 {
            return Err(ApiError::PayloadTooLarge);
        }
        if declared == 0 {
            return Ok(Map::new());
        }
    }
    if !is_json(headers) {
        return Err(ApiError::UnsupportedMediaType("Request body must be application/json".into()));
    }
    let raw = read_body(body, max_body_bytes, timeout).await?;
    if raw.trim_ascii().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::UnprocessableBody("Request body must be a JSON object".into())),
        Err(_) => Err(ApiError::UnprocessableBody("Request body is not valid JSON".into())),
    }
}

/// One entry of a `422 Request validation failed` problem: where and which rule, never the
/// offending input (validation messages can echo it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// Location, e.g. `["body", "type"]`.
    pub loc: Vec<String>,
    /// Rule name (pydantic spelling: `missing`, `literal_error`, `dict_type`, ...).
    pub kind: &'static str,
}

impl ValidationIssue {
    pub(crate) fn new(loc: &[&str], kind: &'static str) -> Self {
        Self { loc: loc.iter().map(|s| (*s).to_owned()).collect(), kind }
    }
}

/// `problem(422, "Request validation failed", errors=[{loc, type}])`, capped at
/// [`MAX_VALIDATION_ERRORS`] entries with `errors_total` when truncated.
#[must_use]
pub fn validation_problem(issues: &[ValidationIssue]) -> Problem {
    let errors: Vec<Value> = issues
        .iter()
        .take(MAX_VALIDATION_ERRORS)
        .map(|i| serde_json::json!({"loc": i.loc, "type": i.kind}))
        .collect();
    let problem = Problem::new(422, "Request validation failed").ext("errors", errors);
    if issues.len() > MAX_VALIDATION_ERRORS {
        return problem.ext("errors_total", issues.len());
    }
    problem
}

/// A body read the way the `FastAPI` router's route class does, size and time bounded, with
/// one hardening: `Content-Type: application/json` is required **even for an empty body**
/// (415 otherwise). An empty-bodied POST would otherwise be a CORS "simple request" that a
/// cross-site form can send (X0.7, CSRF). `Ok(None)` for an empty body.
pub(crate) async fn bounded_json(
    headers: &HeaderMap,
    body: Body,
    max_body_bytes: usize,
    timeout: Duration,
) -> Result<Option<Value>, Problem> {
    if declared_length(headers).is_some_and(|n| n > max_body_bytes as u64) {
        return Err(ApiError::PayloadTooLarge.to_problem());
    }
    if !is_json(headers) {
        return Err(ApiError::UnsupportedMediaType("Request body must be application/json".into())
            .to_problem());
    }
    let raw = read_body(body, max_body_bytes, timeout).await.map_err(|e| e.to_problem())?;
    if raw.trim_ascii().is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|_| validation_problem(&[ValidationIssue::new(&["body"], "json_invalid")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_header_rules() {
        let mut headers = HeaderMap::new();
        assert!(matches!(idempotency_key_from(&headers), Ok(None)));
        headers.insert(IDEMPOTENCY_HEADER, "k-1".parse().unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(idempotency_key_from(&headers).ok().flatten().as_deref(), Some("k-1"));
        let long = "x".repeat(300);
        headers.insert(IDEMPOTENCY_HEADER, long.parse().unwrap_or_else(|e| panic!("{e}")));
        assert!(matches!(idempotency_key_from(&headers), Err(ApiError::BadRequest(_))));
    }

    #[test]
    fn reserved_keys() {
        let mut payload = Map::new();
        assert!(refuse_reserved_send_keys(&payload).is_ok());
        payload.insert("priority".into(), Value::Bool(true));
        assert!(matches!(refuse_reserved_send_keys(&payload), Err(ApiError::ReservedKey)));
        // Adapter-owned: the key comes only from the validated header (Python CW-04).
        let mut payload = Map::new();
        payload.insert("idempotency_key".into(), Value::from("k"));
        assert!(matches!(refuse_reserved_send_keys(&payload), Err(ApiError::ReservedKey)));
        // `id` is ordinary event data and stays allowed (the inbox may fall back to it).
        let mut payload = Map::new();
        payload.insert("id".into(), Value::from("k"));
        assert!(refuse_reserved_send_keys(&payload).is_ok());
    }

    #[test]
    fn validation_problem_is_capped() {
        let issues = vec![ValidationIssue::new(&["body"], "missing"); 60];
        let body = validation_problem(&issues).body();
        assert_eq!(body["errors"].as_array().map(Vec::len), Some(MAX_VALIDATION_ERRORS));
        assert_eq!(body["errors_total"], 60);
    }
}
