//! The live Stately Inspector over WebSocket (Python `contrib/starlette/inspector.py`).
//!
//! An inspector streams every event of every machine the registry runs: a data-exfiltration
//! endpoint by design. So:
//!
//! * it **refuses** to mount unless `debug` is `true` (never in production);
//! * X0.7: a random token compared in constant time, presented as `Authorization: Bearer`,
//!   `X-XSM-Token` or the `HttpOnly` cookie set by the one-time `GET <path>?token=` redirect
//!   (never a persistent query string on the socket); a loopback `Host` only unless
//!   `allow_remote` (the token stays); `Origin` checked with the registry's
//!   [`origin_allowed`](crate::StatechartRegistry::origin_allowed);
//! * context is deny-by-default (`context_allowlist`), exactly as in the core observer.
//!
//! Routes (both on `path`): `GET` with `Upgrade: websocket` streams one protocol message per
//! text frame (`@xstate.actor` / `.event` / `.snapshot`, what `@statelyai/inspect`'s
//! `createWebSocketReceiver` parses); `GET ?token=…` sets the cookie and answers 303 → `path`;
//! a plain `GET` returns the recorded backlog as JSON. Python's starlette inspector serves
//! this JSON backlog (not an HTML page), and so does this port.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;
use xstate_core::Observer;
use xstate_inspect::InspectorObserver;

use crate::problem::problem;
use crate::registry::{RegistryError, StatechartRegistry};
use crate::routes::ws::{WS_POLICY_VIOLATION, close};

mod sink;

use sink::SEND_TIMEOUT;
pub use sink::{COOKIE_NAME, WebSocketSink, new_token};

/// Default mount path.
pub const DEFAULT_INSPECTOR_PATH: &str = "/_xsm/inspect";

/// `Host` names accepted without `allow_remote`.
#[cfg(not(test))]
const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "::1"];
/// `Host` names accepted without `allow_remote`; unit tests add the `testserver` placeholder.
#[cfg(test)]
const LOOPBACK: [&str; 4] = ["127.0.0.1", "localhost", "::1", "testserver"];

/// Options of [`inspector_router`].
#[derive(Debug, Clone)]
pub struct InspectorOptions {
    /// Mount path (default [`DEFAULT_INSPECTOR_PATH`]).
    pub path: String,
    /// Must be `true`, or mounting fails with [`RegistryError::NotDebug`].
    pub debug: bool,
    /// The token (default: 32 random bytes, URL-safe base64). A configured token must use
    /// only `A-Za-z0-9-._~`.
    pub token: Option<String>,
    /// Context keys that may leave the process (default none).
    pub context_allowlist: Vec<String>,
    /// Send allow-listed event payload fields (default `false`).
    pub include_payloads: bool,
    /// Payload keys sent when `include_payloads` is on.
    pub payload_allowlist: Vec<String>,
    /// Accept a non-loopback `Host` (the token is still required).
    pub allow_remote: bool,
    /// Messages replayed to a new client (default 1000).
    pub history: usize,
    /// Per-client backlog before it is cut off (default 10 000).
    pub max_queue: usize,
}

impl Default for InspectorOptions {
    fn default() -> Self {
        Self {
            path: DEFAULT_INSPECTOR_PATH.to_owned(),
            debug: false,
            token: None,
            context_allowlist: Vec::new(),
            include_payloads: false,
            payload_allowlist: Vec::new(),
            allow_remote: false,
            history: 1000,
            max_queue: 10_000,
        }
    }
}

struct InspectorCtx {
    registry: Arc<StatechartRegistry>,
    sink: Arc<WebSocketSink>,
    path: String,
    allow_remote: bool,
}

fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    if host.matches(':').count() == 1 {
        return host.rsplit_once(':').map_or(host, |(name, _)| name);
    }
    host
}

impl InspectorCtx {
    fn host_ok(&self, headers: &HeaderMap) -> bool {
        if self.allow_remote {
            return true;
        }
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        LOOPBACK.contains(&host_name(host))
    }

    /// `GET <path>`: redeem `?token=` for the cookie, else the backlog (token required).
    fn http_view(&self, headers: &HeaderMap, query: &HashMap<String, String>) -> Response {
        if !self.host_ok(headers) {
            return problem(421, "Misdirected Request").detail("loopback only").into_response();
        }
        if !self.registry.origin_allowed(headers) {
            return problem(403, "Forbidden")
                .detail("cross-origin request refused")
                .into_response();
        }
        let unauthorized =
            || problem(401, "Unauthorized").detail("inspector token").into_response();
        if let Some(once) = query.get("token") {
            if !self.sink.token_ok(Some(once)) {
                return unauthorized();
            }
            return self.redirect_with_cookie();
        }
        if !self.sink.token_ok(WebSocketSink::presented_token(headers).as_deref()) {
            return unauthorized();
        }
        let mut response = axum::Json(self.sink.messages()).into_response();
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }

    /// The `HttpOnly; SameSite=strict` cookie carrying the token (`Secure` too when the
    /// inspector is reachable remotely: never send the token over plain HTTP off-host).
    fn cookie(&self) -> String {
        let secure = if self.allow_remote { "; Secure" } else { "" };
        format!(
            "{COOKIE_NAME}={}; HttpOnly; Path={}; SameSite=strict{secure}",
            self.sink.token(),
            self.path
        )
    }

    /// 303 → `path` setting the cookie. Fails closed: a header that cannot be built is a 500
    /// problem, never a redirect without the cookie or the `Location`.
    fn redirect_with_cookie(&self) -> Response {
        let (Ok(location), Ok(cookie)) =
            (HeaderValue::from_str(&self.path), HeaderValue::from_str(&self.cookie()))
        else {
            tracing::error!(target: "xstate::axum", path = %self.path, "inspector redirect headers are not valid header values");
            return problem(500, "Internal Server Error").into_response();
        };
        let mut response = StatusCode::SEE_OTHER.into_response();
        let headers = response.headers_mut();
        headers.insert(header::LOCATION, location);
        headers.insert(header::SET_COOKIE, cookie);
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        response
    }
}

async fn view(
    State(ctx): State<Arc<InspectorCtx>>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Ok(upgrade) = upgrade else { return ctx.http_view(&headers, &query) };
    let admitted = ctx.host_ok(&headers)
        && ctx.registry.origin_allowed(&headers)
        && ctx.sink.token_ok(WebSocketSink::presented_token(&headers).as_deref());
    upgrade.on_upgrade(move |mut socket| async move {
        if admitted {
            ctx.sink.serve(socket).await;
        } else {
            close(&mut socket, WS_POLICY_VIOLATION, SEND_TIMEOUT).await;
        }
    })
}

/// `true` when `token` can travel verbatim in a URL query, a cookie value and a header:
/// non-empty and only RFC 3986 unreserved characters (`A-Za-z0-9-._~`), the rule of
/// `xstate_inspect`'s SSE server.
fn token_is_well_formed(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Mount the live inspector: attaches an [`InspectorObserver`] (session id = the instance's
/// store key) to every interpreter the registry builds from now on and returns the router
/// serving `options.path` plus the sink (read [`WebSocketSink::token`] to hand to the
/// developer).
///
/// # Errors
/// [`RegistryError::NotDebug`] unless `options.debug` is `true`;
/// [`RegistryError::InvalidSetting`] for a configured (non-empty) token outside
/// `A-Za-z0-9-._~` (it must fit a URL, a cookie and a header unescaped). An empty token means
/// "generate one", as `None` does.
pub fn inspector_router(
    registry: &Arc<StatechartRegistry>,
    options: InspectorOptions,
) -> Result<(Router, Arc<WebSocketSink>), RegistryError> {
    let InspectorOptions {
        path,
        debug,
        token,
        context_allowlist,
        include_payloads,
        payload_allowlist,
        allow_remote,
        history,
        max_queue,
    } = options;
    if !debug {
        return Err(RegistryError::NotDebug);
    }
    if let Some(token) = token.as_deref().filter(|t| !t.is_empty())
        && !token_is_well_formed(token)
    {
        return Err(RegistryError::InvalidSetting(
            "the inspector token must use only A-Za-z0-9-._~".into(),
        ));
    }
    let sink = Arc::new(WebSocketSink::new(token, history, max_queue));
    let shared: Arc<dyn xstate_inspect::Sink> = sink.clone();
    registry.add_observer_factory(Arc::new(move |store_key: &str| -> Arc<dyn Observer<Value>> {
        let observer = InspectorObserver::with_shared_sink(Arc::clone(&shared))
            .session_id(store_key)
            .context_allowlist(context_allowlist.clone())
            .payload_allowlist(payload_allowlist.clone());
        let observer = if include_payloads { observer.include_payloads(true) } else { observer };
        Arc::new(observer)
    }));
    let router = Router::new().route(&path, get(view));
    let ctx = Arc::new(InspectorCtx {
        registry: Arc::clone(registry),
        sink: Arc::clone(&sink),
        path,
        allow_remote,
    });
    let router = router.with_state(ctx);
    Ok((router, sink))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names() {
        assert_eq!(host_name("localhost:8000"), "localhost");
        assert_eq!(host_name("[::1]:80"), "::1");
        assert_eq!(host_name("::1"), "::1");
        assert_eq!(host_name("example.com"), "example.com");
    }

    #[test]
    fn token_charset() {
        assert!(token_is_well_formed("Ab0-._~"));
        assert!(token_is_well_formed(&new_token()));
        for bad in ["", "a b", "a;b", "a=b", "a,b", "a\r\nSet-Cookie: x", "é"] {
            assert!(!token_is_well_formed(bad), "{bad:?}");
        }
    }
}
