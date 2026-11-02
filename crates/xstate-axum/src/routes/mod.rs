//! The HTTP surface of one machine (Python `StatechartRouter`).
//!
//! | Method | Path | |
//! |---|---|---|
//! | `GET` | `{prefix}/{id}` | state body + `machine_version` |
//! | `POST` | `{prefix}/{id}/send` | `{"type", "payload"}` → receipt |
//! | `POST` | `{prefix}/{id}/events/{EVENT}` | one route per declared event; optional object body |
//! | `GET` | `{prefix}/{id}/events` | `{"available", "declared"}` |
//! | `GET` | `{prefix}/{id}/diagram.mmd` | Mermaid `stateDiagram-v2` (optional) |
//! | `GET` | `{prefix}/{id}/stream` | SSE: `snapshot`, then each `transition` |
//! | `GET` (upgrade) | `{prefix}/{id}/ws` | WebSocket: send events, receive transitions |
//!
//! `{prefix}` defaults to `/{name}`. Every route runs the machine's authorizer (X0.1); bodies
//! are JSON-only and bounded (X0.7); failures are RFC 9457 problems.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::http::request::Parts;
use axum::routing::{get, post};

use crate::problem::{ApiError, Problem};
use crate::receipt::declared_events;
use crate::registry::{Registration, RegistryError, StatechartRegistry};

mod diagram;
mod events;
mod get;
mod send;
pub(crate) mod stream;
pub(crate) mod ws;

/// An extra check on one event's dedicated route (Python `per_event_dependencies`). Such an
/// event is refused on `/send` (403) so the gate cannot be bypassed.
pub trait EventGate: Send + Sync {
    /// Admit (`Ok`) or answer with the problem.
    ///
    /// # Errors
    /// The problem to answer with.
    fn check(&self, parts: &Parts) -> Result<(), Problem>;
}

impl<F> EventGate for F
where
    F: Fn(&Parts) -> Result<(), Problem> + Send + Sync,
{
    fn check(&self, parts: &Parts) -> Result<(), Problem> {
        self(parts)
    }
}

/// Options of [`router`].
#[derive(Clone)]
pub struct RouterOptions {
    /// Path prefix (default `/{name}`).
    pub prefix: Option<String>,
    /// Serve `GET /{id}/diagram.mmd` (default `true`).
    pub include_diagram: bool,
    /// `false`: reads, sends and streams on an unknown key are 404 (WebSocket: close 1008)
    /// instead of starting a fresh instance (default `true`).
    pub create_if_missing: bool,
    /// `{EVENT: gate}` checked on that event's `/events/EVENT` route; the event is refused
    /// on `/send`.
    pub per_event_gates: HashMap<String, Arc<dyn EventGate>>,
}

impl Default for RouterOptions {
    fn default() -> Self {
        Self {
            prefix: None,
            include_diagram: true,
            create_if_missing: true,
            per_event_gates: HashMap::new(),
        }
    }
}

impl fmt::Debug for RouterOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouterOptions")
            .field("prefix", &self.prefix)
            .field("include_diagram", &self.include_diagram)
            .field("create_if_missing", &self.create_if_missing)
            .field("per_event_gates", &self.per_event_gates.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Shared state of one machine's routes.
pub(crate) struct RouteCtx {
    pub(crate) registry: Arc<StatechartRegistry>,
    pub(crate) name: String,
    pub(crate) reg: Arc<Registration>,
    pub(crate) events: Vec<String>,
    pub(crate) create_if_missing: bool,
    pub(crate) gated: HashMap<String, Arc<dyn EventGate>>,
}

impl RouteCtx {
    /// Authorize a read (`event = None`) and, with `create_if_missing = false`, require the
    /// instance to exist (404).
    pub(crate) async fn guard_read(&self, parts: &Parts, key: &str) -> Result<(), ApiError> {
        let principal = self.registry.principal_of(parts);
        self.registry.authorize(principal.as_deref(), &self.name, key, None)?;
        self.require_existing(key).await
    }

    pub(crate) async fn require_existing(&self, key: &str) -> Result<(), ApiError> {
        if !self.create_if_missing && !self.registry.exists(&self.name, key).await? {
            return Err(xstate_persistence::StoreError::NotFound(key.to_owned()).into());
        }
        Ok(())
    }
}

/// An event name that can be a literal path segment.
fn routable(event: &str) -> bool {
    !event.starts_with([':', '*']) && !event.contains(['/', '{', '}'])
}

/// Build the routes of machine `name`:
///
/// | Method | Path | |
/// |---|---|---|
/// | `GET` | `{prefix}/{id}` | state body + `machine_version` |
/// | `POST` | `{prefix}/{id}/send` | `{"type", "payload"}` → receipt |
/// | `POST` | `{prefix}/{id}/events/{EVENT}` | one route per declared event; optional object body |
/// | `GET` | `{prefix}/{id}/events` | `{"available", "declared"}` |
/// | `GET` | `{prefix}/{id}/diagram.mmd` | Mermaid `stateDiagram-v2` (optional) |
/// | `GET` | `{prefix}/{id}/stream` | SSE: `snapshot`, then each `transition` |
/// | `GET` (upgrade) | `{prefix}/{id}/ws` | WebSocket: send events, receive transitions |
///
/// `{prefix}` defaults to `/{name}`. Every route runs the machine's authorizer (X0.1); bodies
/// are JSON-only and bounded (X0.7); failures are RFC 9457 problems.
///
/// # Errors
/// [`RegistryError::UnknownMachine`]; [`RegistryError::UnknownGatedEvents`] when a gate names
/// an event the machine does not declare (battle #276-a: silent acceptance is a bug).
pub fn router(
    registry: &Arc<StatechartRegistry>,
    name: &str,
    options: RouterOptions,
) -> Result<Router, RegistryError> {
    let reg = registry
        .regs
        .get(name)
        .cloned()
        .ok_or_else(|| RegistryError::UnknownMachine(name.to_owned()))?;
    let events = declared_events(&reg.machine);
    let mut unknown: Vec<String> =
        options.per_event_gates.keys().filter(|e| !events.contains(e)).cloned().collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(RegistryError::UnknownGatedEvents {
            machine: name.to_owned(),
            events: unknown,
        });
    }
    let base = options.prefix.clone().unwrap_or_else(|| format!("/{name}"));
    let base = base.trim_end_matches('/').to_owned();
    let ctx = Arc::new(RouteCtx {
        registry: Arc::clone(registry),
        name: name.to_owned(),
        reg,
        events: events.clone(),
        create_if_missing: options.create_if_missing,
        gated: options.per_event_gates,
    });
    let mut router = Router::new()
        .route(&format!("{base}/{{id}}"), get(get::get_state))
        .route(&format!("{base}/{{id}}/send"), post(send::send_any))
        .route(&format!("{base}/{{id}}/events"), get(events::list_events))
        .route(&format!("{base}/{{id}}/stream"), get(stream::stream))
        .route(&format!("{base}/{{id}}/ws"), get(ws::websocket));
    for event in events {
        if !routable(&event) {
            tracing::warn!(target: "xstate::axum", machine = name, %event, "event name cannot be a path segment; only /send accepts it");
            continue;
        }
        let path = format!("{base}/{{id}}/events/{event}");
        router = router.route(&path, send::event_route(event));
    }
    if options.include_diagram {
        router = router.route(&format!("{base}/{{id}}/diagram.mmd"), get(diagram::diagram));
    }
    Ok(router.with_state(ctx))
}
