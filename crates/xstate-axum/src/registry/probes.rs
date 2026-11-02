//! Liveness / readiness probes (Python `_ProbesMixin`).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use super::StatechartRegistry;
use crate::problem::problem;

/// Default liveness path.
pub const HEALTH_PATH: &str = "/_xsm/health";
/// Default readiness path.
pub const READY_PATH: &str = "/_xsm/ready";

/// Longest the readiness probe waits for the store's health check before answering
/// not-ready (a hung store must not hang the probe).
const READY_TIMEOUT: Duration = Duration::from_secs(2);

async fn health() -> Response {
    axum::Json(json!({"status": "ok"})).into_response()
}

async fn ready(State(registry): State<Arc<StatechartRegistry>>) -> Response {
    if !registry.is_started() || registry.is_draining() {
        return problem(503, "Not Ready").into_response();
    }
    let store = Arc::clone(registry.store());
    let check = tokio::task::spawn_blocking(move || store.health());
    let Ok(health) = tokio::time::timeout(READY_TIMEOUT, check).await else {
        tracing::warn!(target: "xstate::axum", timeout = ?READY_TIMEOUT, "store health check timed out; not ready");
        return problem(503, "Store unavailable").ext("error", "TimeoutError").into_response();
    };
    match health {
        Ok(Ok(info)) if info.ok => axum::Json(json!({
            "status": "ready",
            "residents": 0,
            "timers": registry.timers_running(),
        }))
        .into_response(),
        Ok(Ok(_)) => problem(503, "Store unavailable").into_response(),
        Ok(Err(error)) => {
            let name = crate::problem::ApiError::from(error).error_name();
            problem(503, "Store unavailable").ext("error", name).into_response()
        }
        Err(_) => problem(503, "Store unavailable").ext("error", "RuntimeError").into_response(),
    }
}

impl StatechartRegistry {
    /// `GET /_xsm/health` (200 while the process answers) and `GET /_xsm/ready` (200 once
    /// [`start`](Self::start)ed, not draining and the store reports healthy within 2 s; else a
    /// 503 problem).
    pub fn probes_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(HEALTH_PATH, get(health))
            .route(READY_PATH, get(ready))
            .with_state(Arc::clone(self))
    }
}
