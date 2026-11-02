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

/// Title of the 403 refusing a gated event anywhere but its dedicated route.
pub(crate) const GATED_TITLE: &str = "Use this event's dedicated route";

/// The `/send` and WebSocket rule: an event must be declared by the machine (any event is
/// accepted when the machine declares none).
pub(crate) fn is_declared(events: &[String], event: &str) -> bool {
    events.is_empty() || events.iter().any(|e| e == event)
}

/// A prefix axum can mount without panicking: `/`-led, no braces (path parameters), no
/// whitespace or control characters, no segment starting with `:` or `*`.
fn valid_prefix(prefix: &str) -> bool {
    prefix.starts_with('/')
        && !prefix.contains(['{', '}'])
        && !prefix.chars().any(|c| c.is_whitespace() || c.is_control())
        && !prefix.split('/').any(|segment| segment.starts_with([':', '*']))
}

/// Refuse gates naming events the machine does not declare (battle #276-a).
fn check_gates(
    name: &str,
    events: &[String],
    gates: &HashMap<String, Arc<dyn EventGate>>,
) -> Result<(), RegistryError> {
    let mut unknown: Vec<String> = gates.keys().filter(|e| !events.contains(e)).cloned().collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    Err(RegistryError::UnknownGatedEvents { machine: name.to_owned(), events: unknown })
}

/// The validated mount prefix (default `/{name}`), without a trailing `/`.
fn mount_base(prefix: Option<&str>, name: &str) -> Result<String, RegistryError> {
    let base = prefix.map_or_else(|| format!("/{name}"), str::to_owned);
    if !valid_prefix(&base) {
        return Err(RegistryError::InvalidSetting(format!(
            "router prefix {base:?} must start with '/' and contain no braces, whitespace or \
             ':' / '*' segments"
        )));
    }
    Ok(base.trim_end_matches('/').to_owned())
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
/// an event the machine does not declare (battle #276-a: silent acceptance is a bug);
/// [`RegistryError::InvalidSetting`] for a prefix that is not `/`-led or contains braces,
/// whitespace or a `:` / `*` segment (axum would panic on it).
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
    check_gates(name, &events, &options.per_event_gates)?;
    let base = mount_base(options.prefix.as_deref(), name)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_rules() {
        for good in ["/", "/orders", "/api/v1/orders/", "/a-b_c.d~e"] {
            assert!(valid_prefix(good), "{good:?}");
        }
        for bad in ["", "orders", "/a/{id}", "/a}", "/a b", "/a\tb", "/a/:x", "/a/*rest"] {
            assert!(!valid_prefix(bad), "{bad:?}");
        }
    }

    #[test]
    fn declared_rule() {
        let events = vec!["GO".to_owned()];
        assert!(is_declared(&events, "GO"));
        assert!(!is_declared(&events, "STOP"));
        assert!(is_declared(&[], "ANY"), "a machine declaring no events accepts any");
    }
}
