//! RFC 9457 problem details and the error → HTTP status mapping (Python `_http.py`).
//!
//! X0.7: a problem carries a fixed **title** and the Python exception **class name** in its
//! `error` member, never the error's message: messages can echo keys, payload fragments or file
//! paths. Every request-level failure of this crate is an [`ApiError`]; [`ApiError::to_problem`]
//! renders it with the same `type` / `title` / `status` strings as the Python adapters.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use xstate_core::ErrorKind;
use xstate_persistence::StoreError;
use xstate_persistence::receipts::ReceiptErrorKind;

/// Media type of every problem response.
pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";

/// An RFC 9457 `application/problem+json` document.
///
/// The body is `{"type": "about:blank", "title", "status"[, "detail"], ...extensions}`, in that
/// key order.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// HTTP status.
    pub status: u16,
    /// Short, fixed, human-readable summary. Never exception text.
    pub title: String,
    /// Optional caller-authored explanation.
    pub detail: Option<String>,
    /// Extension members (`error`, `machine_version`, `errors`, ...).
    pub ext: Vec<(String, Value)>,
}

impl Problem {
    /// A problem with no detail and no extension members.
    #[must_use]
    pub fn new(status: u16, title: impl Into<String>) -> Self {
        Self { status, title: title.into(), detail: None, ext: Vec::new() }
    }

    /// Set the `detail` member.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Add an extension member (replacing one with the same name).
    #[must_use]
    pub fn ext(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        self.ext.retain(|(k, _)| *k != key);
        self.ext.push((key, value.into()));
        self
    }

    /// The JSON body.
    #[must_use]
    pub fn body(&self) -> Value {
        let mut body = Map::new();
        body.insert("type".into(), Value::from("about:blank"));
        body.insert("title".into(), Value::from(self.title.clone()));
        body.insert("status".into(), Value::from(self.status));
        if let Some(detail) = &self.detail {
            body.insert("detail".into(), Value::from(detail.clone()));
        }
        for (k, v) in &self.ext {
            body.insert(k.clone(), v.clone());
        }
        Value::Object(body)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, self.body().to_string()).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_MEDIA_TYPE));
        response
    }
}

/// Shorthand for [`Problem::new`] (Python `problem(status, title)`).
#[must_use]
pub fn problem(status: u16, title: impl Into<String>) -> Problem {
    Problem::new(status, title)
}

/// A request-level failure. Each variant knows its status, its public title and the Python
/// class name reported in the problem's `error` member.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// The request is malformed (400, `BadRequestError`); the title says how.
    #[error("{0}")]
    BadRequest(String),
    /// The authorizer refused (403, `ForbiddenError`).
    #[error("Forbidden")]
    Forbidden,
    /// The principal resolver identified nobody (401, `UnauthenticatedError`).
    #[error("{0}")]
    Unauthenticated(String),
    /// The registry is draining (503, `ShuttingDownError`); retryable elsewhere.
    #[error("Shutting down")]
    ShuttingDown,
    /// The body exceeds `max_body_bytes` (413, `PayloadTooLargeError`).
    #[error("Payload Too Large")]
    PayloadTooLarge,
    /// The body did not arrive within `body_timeout` (408, `RequestTimeoutError`).
    #[error("Request Timeout")]
    RequestTimeout,
    /// The body is not `application/json` (415, `UnsupportedMediaTypeError`).
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// An `Idempotency-Key` reached a registry without an inbox (501).
    #[error("Idempotency-Key not supported: no idempotency inbox configured")]
    IdempotencyNotConfigured,
    /// The body is not valid JSON / not an object (422, `UnprocessableBodyError`).
    #[error("{0}")]
    UnprocessableBody(String),
    /// The payload names a `send()` option (422, `ReservedKeyError`).
    #[error("Reserved key in payload")]
    ReservedKey,
    /// The stored snapshot belongs to another chart revision (409, `SnapshotDriftError`).
    #[error("snapshot drift: {message}")]
    SnapshotDrift {
        /// The running machine's version label (the hint a client / operator needs).
        machine_version: Option<String>,
        /// Engine message (logged, never sent).
        message: String,
    },
    /// The idempotency key was used for a different event (422).
    #[error("idempotency key reused with a different payload")]
    IdempotencyMismatch,
    /// The first delivery with this idempotency key is still running (409).
    #[error("idempotency key in flight")]
    IdempotencyInFlight,
    /// No machine is registered under this name (500, Python `KeyError`).
    #[error("no machine registered as '{0}'")]
    UnknownMachine(String),
    /// A blocking task panicked or was cancelled (500).
    #[error("internal error: {0}")]
    Internal(String),
    /// A store failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An engine failure raised before the event was processed.
    #[error(transparent)]
    Engine(#[from] xstate_core::Error),
}

/// `(status, title, Python class name)` of an engine error (`_STATUS_TABLE`).
fn engine_entry(error: &xstate_core::Error) -> (u16, &'static str, String) {
    let name = || ReceiptErrorKind::from_engine(error.kind()).type_name().to_owned();
    match error.kind() {
        ErrorKind::UnknownEvent => (422, "Unknown event", name()),
        ErrorKind::InvalidEvent => (422, "Invalid event", name()),
        ErrorKind::InvalidLifecycle => (409, "Instance is no longer running", name()),
        ErrorKind::Snapshot if is_drift(error) => {
            (409, "Machine version mismatch", "SnapshotDriftError".into())
        }
        _ => (500, "Internal Server Error", name()),
    }
}

/// `true` when a snapshot error says the snapshot belongs to another machine or revision
/// (Python `SnapshotDriftError`) rather than being corrupt.
#[must_use]
pub fn is_drift(error: &xstate_core::Error) -> bool {
    const DRIFT: [&str; 3] =
        ["was written by version", "machine structure changed", "snapshot belongs to machine"];
    matches!(error, xstate_core::Error::Snapshot(msg) if DRIFT.iter().any(|d| msg.contains(d)))
}

/// `(status, title, Python class name)` of a store error.
fn store_entry(error: &StoreError) -> (u16, &'static str, String) {
    let (status, title, name) = match error {
        StoreError::Engine(engine) => return engine_entry(engine),
        StoreError::Conflict { .. } => (409, "Conflict", "ConflictError"),
        StoreError::LockTimeout { .. } => (409, "Lock timeout", "LockTimeoutError"),
        StoreError::Unavailable(_) => (503, "Store unavailable", "StoreUnavailableError"),
        StoreError::NotFound(_) => (404, "Not Found", "KeyNotFoundError"),
        StoreError::InvalidKey(_) => (400, "Invalid key", "InvalidKeyError"),
        StoreError::SnapshotTooLarge { .. } => {
            (500, "Internal Server Error", "SnapshotTooLargeError")
        }
        StoreError::Corrupt(_) => (500, "Internal Server Error", "SnapshotCorruptError"),
        StoreError::Io(_) => (500, "Internal Server Error", "OSError"),
        _ => (500, "Internal Server Error", "StoreError"),
    };
    (status, title, name.to_owned())
}

impl ApiError {
    /// `(status, title, class name)`.
    fn entry(&self) -> (u16, String, String) {
        let fixed = |status: u16, title: &str, name: &str| (status, title.to_owned(), name.into());
        match self {
            Self::BadRequest(title) => fixed(400, title, "BadRequestError"),
            Self::Forbidden => fixed(403, "Forbidden", "ForbiddenError"),
            Self::Unauthenticated(title) => fixed(401, title, "UnauthenticatedError"),
            Self::ShuttingDown => fixed(503, "Shutting down", "ShuttingDownError"),
            Self::PayloadTooLarge => fixed(413, "Payload Too Large", "PayloadTooLargeError"),
            Self::RequestTimeout => fixed(408, "Request Timeout", "RequestTimeoutError"),
            Self::UnsupportedMediaType(title) => fixed(415, title, "UnsupportedMediaTypeError"),
            Self::IdempotencyNotConfigured => fixed(
                501,
                "Idempotency-Key not supported: no idempotency inbox configured",
                "IdempotencyNotConfiguredError",
            ),
            Self::UnprocessableBody(title) => fixed(422, title, "UnprocessableBodyError"),
            Self::ReservedKey => fixed(422, "Reserved key in payload", "ReservedKeyError"),
            Self::SnapshotDrift { .. } => {
                fixed(409, "Machine version mismatch", "SnapshotDriftError")
            }
            Self::IdempotencyMismatch => {
                fixed(422, "Idempotency key reused", "IdempotencyMismatchError")
            }
            Self::IdempotencyInFlight => {
                fixed(409, "Request in flight", "IdempotencyInFlightError")
            }
            Self::UnknownMachine(_) => fixed(500, "Internal Server Error", "KeyError"),
            Self::Internal(_) => fixed(500, "Internal Server Error", "RuntimeError"),
            Self::Store(e) => {
                let (s, t, n) = store_entry(e);
                (s, t.to_owned(), n)
            }
            Self::Engine(e) => {
                let (s, t, n) = engine_entry(e);
                (s, t.to_owned(), n)
            }
        }
    }

    /// HTTP status (Python `status_for_exception`; 500 when unknown).
    #[must_use]
    pub fn status(&self) -> u16 {
        self.entry().0
    }

    /// The Python exception class name reported in the problem's `error` member.
    #[must_use]
    pub fn error_name(&self) -> String {
        self.entry().2
    }

    /// The problem document (Python `problem_for_exception`): fixed title + class name, plus
    /// `machine_version` for a snapshot drift.
    #[must_use]
    pub fn to_problem(&self) -> Problem {
        let (status, title, name) = self.entry();
        let problem = Problem::new(status, title).ext("error", name);
        match self {
            Self::SnapshotDrift { machine_version, .. } => {
                problem.ext("machine_version", machine_version.clone())
            }
            _ => problem,
        }
    }

    /// Re-classify an engine snapshot drift as [`ApiError::SnapshotDrift`] carrying the running
    /// machine's version; every other error is returned unchanged.
    #[must_use]
    pub fn with_machine_version(self, version: Option<&str>) -> Self {
        let drift = match &self {
            Self::Engine(e) | Self::Store(StoreError::Engine(e)) if is_drift(e) => e.to_string(),
            _ => return self,
        };
        Self::SnapshotDrift { machine_version: version.map(str::to_owned), message: drift }
    }
}

/// HTTP status for an error (Python `status_for_exception`).
#[must_use]
pub fn status_for_error(error: &ApiError) -> u16 {
    error.status()
}

/// The problem document for an error (Python `problem_for_exception`).
#[must_use]
pub fn problem_for_error(error: &ApiError) -> Problem {
    error.to_problem()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status() >= 500 && !matches!(self, Self::ShuttingDown) {
            tracing::warn!(target: "xstate::axum", error = %self, "request failed");
        }
        self.to_problem().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_body_key_order() {
        let body = Problem::new(404, "Not Found").detail("x").ext("error", "E").body();
        let keys: Vec<&String> = body.as_object().map(|m| m.keys().collect()).unwrap_or_default();
        assert_eq!(keys, ["type", "title", "status", "detail", "error"]);
    }

    #[test]
    fn store_errors_map_like_python() {
        let conflict =
            ApiError::Store(StoreError::Conflict { key: "k".into(), expected: 1, actual: Some(2) });
        assert_eq!((conflict.status(), conflict.error_name().as_str()), (409, "ConflictError"));
        let down = ApiError::Store(StoreError::Unavailable("x".into()));
        assert_eq!(down.to_problem().title, "Store unavailable");
        let unknown = ApiError::Engine(xstate_core::Error::UnknownEvent {
            event: "X".into(),
            machine: "m".into(),
        });
        assert_eq!((unknown.status(), unknown.to_problem().title.as_str()), (422, "Unknown event"));
    }

    #[test]
    fn drift_carries_machine_version() {
        let err = ApiError::Engine(xstate_core::Error::Snapshot(
            "snapshot of machine 'm' was written by version '1'".into(),
        ))
        .with_machine_version(Some("2"));
        let body = err.to_problem().body();
        assert_eq!(body["status"], 409);
        assert_eq!(body["machine_version"], "2");
        assert_eq!(body["error"], "SnapshotDriftError");
    }
}
