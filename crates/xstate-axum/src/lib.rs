//! axum integration for [`xstate_core`] machines: a port of the Python library's
//! `contrib/starlette` (registry, HTTP semantics, SSE / WebSocket streaming, inspector) and
//! `contrib/fastapi/router.py` (the per-machine REST surface).
//!
//! * [`StatechartRegistry`]: named machines over one [`StateStore`]; every request is one
//!   create → act → persist → discard cycle through [`xstate_persistence::persisted`], with
//!   the authorizer (X0.1), the principal-scoped idempotency inbox (X0.2), a post-save fan-out
//!   to stream subscribers, probes, the durable-timer scanner and a graceful drain.
//! * [`router`]: `GET /{name}/{id}`, `POST /{name}/{id}/send`, `POST /{name}/{id}/events/{E}`,
//!   `GET /{name}/{id}/events`, `GET /{name}/{id}/diagram.mmd`, `GET /{name}/{id}/stream`
//!   (SSE) and `GET /{name}/{id}/ws` (WebSocket).
//! * [`problem()`] / [`ApiError`]: RFC 9457 problem details with the Python titles and class
//!   names, never exception text (X0.7).
//! * [`inspector_router`]: the Stately Inspector over WebSocket, debug-only and token-gated.
//!
//! ```no_run
//! use std::sync::Arc;
//! use xstate_axum::{RegisterOptions, RouterOptions, StatechartRegistry, router};
//! use xstate_core::{Machine, MachineLogic};
//! use xstate_persistence::MemoryStore;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let machine = Machine::from_json(
//!     r#"{"id":"order","initial":"pending","states":{"pending":{"on":{"PAY":"paid"}},"paid":{}}}"#,
//! )?;
//! let registry = StatechartRegistry::builder(Arc::new(MemoryStore::new()))
//!     .register("order", machine, MachineLogic::dynamic(), RegisterOptions::default())?
//!     .build()?;
//! registry.start()?;
//! let app = router(&registry, "order", RouterOptions::default())?.merge(registry.probes_router());
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8000").await?;
//! axum::serve(listener, app).await?;
//! # Ok(()) }
//! ```
//!
//! [`StateStore`]: xstate_persistence::StateStore

#![forbid(unsafe_code)]

pub mod auth;
pub mod body;
pub mod fanout;
pub mod inspector;
pub mod problem;
pub mod receipt;
pub mod registry;
mod routes;

pub use auth::{
    Actor, AllowAll, AuthRequest, Authorizer, Forbidden, PrincipalResolver, allow_all,
    principal_or_401,
};
pub use body::{
    DEFAULT_BODY_TIMEOUT, IDEMPOTENCY_HEADER, RESERVED_SEND_KEYS, idempotency_key_from, json_body,
    refuse_reserved_send_keys,
};
pub use fanout::{FanoutItem, MAX_BACKLOG, Subscribers, Subscription};
pub use inspector::{
    COOKIE_NAME, DEFAULT_INSPECTOR_PATH, InspectorOptions, WebSocketSink, inspector_router,
};
pub use problem::{
    ApiError, PROBLEM_MEDIA_TYPE, Problem, problem, problem_for_error, status_for_error,
};
pub use receipt::{
    ContextSerializer, StateView, declared_events, receipt_body, receipt_to_status, state_body,
};
pub use registry::{
    ConnectionSlot, KEY_SEP, ObserverFactory, OptionsFactory, RegisterOptions, RegistryBuilder,
    RegistryError, SendOutcome, SendRequest, StatechartRegistry,
};
pub use routes::stream::transition_stream;
pub use routes::ws::{
    WS_GOING_AWAY, WS_INTERNAL_ERROR, WS_MESSAGE_TOO_BIG, WS_POLICY_VIOLATION, WS_TRY_AGAIN_LATER,
};
pub use routes::{EventGate, RouterOptions, router};
