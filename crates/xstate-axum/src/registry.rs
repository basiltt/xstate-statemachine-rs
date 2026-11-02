//! [`StatechartRegistry`]: named machines over one store, exposed over HTTP (Python
//! `registry.py`).
//!
//! Under N workers there is no shared memory, so every request is one
//! create → act → persist → discard cycle: load the snapshot, run one [`SyncInterpreter`] over
//! it, save with `expected_version`, throw the interpreter away
//! ([`xstate_persistence::persisted`]). The registry adds the authorizer, the idempotency
//! inbox, the post-save fan-out to SSE / WebSocket subscribers, connection accounting, the
//! durable-timer scanner, readiness probes and a graceful drain.
//!
//! [`SyncInterpreter`]: xstate_core::SyncInterpreter

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use serde_json::Value;
use xstate_core::{Machine, MachineLogic, Observer};
use xstate_persistence::{
    DEFAULT_MAX_SNAPSHOT_BYTES, InboxStore, PersistedOptions, StateStore, StoreError,
};

use crate::auth::{AuthRequest, Authorizer, PrincipalResolver, allow_all};
use crate::body::DEFAULT_BODY_TIMEOUT;
use crate::fanout::Subscribers;
use crate::problem::ApiError;
use crate::receipt::ContextSerializer;

mod cycle;
mod probes;
mod scanner;

pub use cycle::{SendOutcome, SendRequest};
pub use probes::{HEALTH_PATH, READY_PATH};
pub use scanner::NowFn;

/// Separator between machine name and instance key in the store key (`"<name>.<key>"`).
/// Names may not contain it; keys may.
pub const KEY_SEP: char = '.';

/// Builds the [`PersistedOptions`] of one request (lock strategy, clock, version policy).
/// `create_if_missing` and `observers` are overwritten per request.
pub type OptionsFactory = Arc<dyn Fn() -> PersistedOptions<Value> + Send + Sync>;

/// Builds an observer for one interpreter, given its store key (the inspector uses it).
pub type ObserverFactory = Arc<dyn Fn(&str) -> Arc<dyn Observer<Value>> + Send + Sync>;

type Topic = (String, String);

/// Configuration errors (raised while building, never per request).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The machine name is empty or contains [`KEY_SEP`].
    #[error("machine name '{0}' must be non-empty and must not contain '.'")]
    InvalidName(String),
    /// The machine name is already registered.
    #[error("machine name '{0}' is already registered")]
    DuplicateName(String),
    /// No machine is registered under this name.
    #[error("no machine registered as '{0}'")]
    UnknownMachine(String),
    /// A setting is out of range or inconsistent.
    #[error("invalid registry setting: {0}")]
    InvalidSetting(String),
    /// Per-event gates name events the machine does not declare (battle #276-a).
    #[error("per_event_gates names events {events:?} that machine '{machine}' does not declare")]
    UnknownGatedEvents {
        /// Machine name.
        machine: String,
        /// The undeclared events.
        events: Vec<String>,
    },
    /// The inspector refuses to mount without `debug: true`.
    #[error(
        "inspector_router() exposes every event and context; it refuses to mount unless \
         debug is true (never in production)"
    )]
    NotDebug,
    /// The machine logic does not implement every name the machine references.
    #[error(transparent)]
    Engine(#[from] xstate_core::Error),
}

/// How a machine is exposed.
#[derive(Clone)]
pub struct RegisterOptions {
    /// Who may read and drive instances (default [`allow_all`], which warns once).
    pub authorize: Arc<dyn Authorizer>,
    /// When set, bodies include `context` (after dropping `_xsm_` keys); otherwise state only.
    pub context_serializer: Option<ContextSerializer>,
    /// Refuse events no state declares with 422 `Unknown event` (Python `strict=True`).
    pub strict: bool,
}

impl Default for RegisterOptions {
    fn default() -> Self {
        Self { authorize: allow_all(), context_serializer: None, strict: false }
    }
}

impl fmt::Debug for RegisterOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisterOptions")
            .field("context_serializer", &self.context_serializer.is_some())
            .field("strict", &self.strict)
            .finish_non_exhaustive()
    }
}

/// One registered machine.
pub(crate) struct Registration {
    pub(crate) name: String,
    pub(crate) machine: Machine,
    pub(crate) logic: MachineLogic<Value>,
    pub(crate) options: RegisterOptions,
}

impl Registration {
    pub(crate) fn serializer(&self) -> Option<&ContextSerializer> {
        self.options.context_serializer.as_ref()
    }
}

/// Request and stream limits.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) max_body_bytes: usize,
    pub(crate) body_timeout: Duration,
    pub(crate) heartbeat: Duration,
    pub(crate) max_connections_per_key: usize,
    pub(crate) drain_timeout: Duration,
    pub(crate) allowed_origins: HashSet<String>,
    pub(crate) timers: Option<Duration>,
}

/// Builder of a [`StatechartRegistry`].
pub struct RegistryBuilder {
    store: Arc<dyn StateStore>,
    options: OptionsFactory,
    inbox: Option<Arc<dyn InboxStore>>,
    principal: Option<Arc<dyn PrincipalResolver>>,
    settings: Settings,
    scanner_now: Option<NowFn>,
    regs: HashMap<String, Arc<Registration>>,
}

impl fmt::Debug for RegistryBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryBuilder")
            .field("settings", &self.settings)
            .field("machines", &self.regs.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl RegistryBuilder {
    /// Persisted-cycle options (lock strategy, clock, version policy); default
    /// [`PersistedOptions::default`] (optimistic lock, real clock).
    #[must_use]
    pub fn persisted_options(
        mut self,
        factory: impl Fn() -> PersistedOptions<Value> + Send + Sync + 'static,
    ) -> Self {
        self.options = Arc::new(factory);
        self
    }

    /// Deduplicate keyed sends through this inbox, scoped to the caller's principal (X0.2).
    /// Requires a [`principal`](Self::principal) resolver.
    #[must_use]
    pub fn inbox(mut self, inbox: Arc<dyn InboxStore>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Name the authenticated caller of a request.
    #[must_use]
    pub fn principal(mut self, resolver: impl PrincipalResolver + 'static) -> Self {
        self.principal = Some(Arc::new(resolver));
        self
    }

    /// Largest accepted request body / WebSocket frame (default 1 MiB).
    #[must_use]
    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.settings.max_body_bytes = bytes;
        self
    }

    /// Longest a request body may take to arrive (default 30 s; 408 beyond).
    #[must_use]
    pub fn body_timeout(mut self, timeout: Duration) -> Self {
        self.settings.body_timeout = timeout;
        self
    }

    /// SSE heartbeat comment / WebSocket ping interval (default 15 s).
    #[must_use]
    pub fn heartbeat(mut self, interval: Duration) -> Self {
        self.settings.heartbeat = interval;
        self
    }

    /// Concurrent SSE + WebSocket clients per instance (default 16; 429 / 1013 beyond).
    #[must_use]
    pub fn max_connections_per_key(mut self, n: usize) -> Self {
        self.settings.max_connections_per_key = n;
        self
    }

    /// Upper bound on [`StatechartRegistry::shutdown`] (default 10 s).
    #[must_use]
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.settings.drain_timeout = timeout;
        self
    }

    /// Extra `Origin` values accepted on streams besides same-origin (X0.7).
    #[must_use]
    pub fn allowed_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.settings.allowed_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Run a `DueTimerScanner` every `interval` between [`StatechartRegistry::start`] and
    /// [`StatechartRegistry::shutdown`].
    #[must_use]
    pub fn run_timers(mut self, interval: Duration) -> Self {
        self.settings.timers = Some(interval);
        self
    }

    /// Inject the scanner's wall clock (epoch seconds; tests).
    #[must_use]
    pub fn scanner_now(mut self, now: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.scanner_now = Some(Arc::new(now));
        self
    }

    /// Register `machine` (with its `logic`) as `name`.
    ///
    /// # Errors
    /// [`RegistryError::InvalidName`], [`RegistryError::DuplicateName`], or
    /// [`RegistryError::Engine`] when `logic` misses an implementation `machine` references.
    pub fn register(
        mut self,
        name: &str,
        machine: Machine,
        logic: MachineLogic<Value>,
        options: RegisterOptions,
    ) -> Result<Self, RegistryError> {
        if name.is_empty() || name.contains(KEY_SEP) {
            return Err(RegistryError::InvalidName(name.to_owned()));
        }
        if self.regs.contains_key(name) {
            return Err(RegistryError::DuplicateName(name.to_owned()));
        }
        logic.validate(&machine)?;
        let reg = Registration { name: name.to_owned(), machine, logic, options };
        self.regs.insert(name.to_owned(), Arc::new(reg));
        Ok(self)
    }

    /// Validate the settings and build the registry.
    ///
    /// # Errors
    /// [`RegistryError::InvalidSetting`] for a zero heartbeat / body timeout / connection cap,
    /// or an inbox without a principal resolver.
    pub fn build(self) -> Result<Arc<StatechartRegistry>, RegistryError> {
        let s = &self.settings;
        let bad = |m: &str| Err(RegistryError::InvalidSetting(m.to_owned()));
        if s.heartbeat.is_zero() {
            return bad("heartbeat must be > 0");
        }
        if s.body_timeout.is_zero() {
            return bad("body_timeout must be > 0");
        }
        if s.max_connections_per_key < 1 {
            return bad("max_connections_per_key must be >= 1");
        }
        if self.settings.timers.is_some_and(|t| t.is_zero()) {
            return bad("the timer scan interval must be > 0");
        }
        if self.inbox.is_some() && self.principal.is_none() {
            return bad("an idempotency inbox needs a principal resolver (X0.2)");
        }
        Ok(Arc::new(StatechartRegistry {
            store: self.store,
            options: self.options,
            inbox: self.inbox,
            principal: self.principal,
            settings: self.settings,
            scanner_now: self.scanner_now,
            regs: Arc::new(self.regs),
            observers: RwLock::new(Vec::new()),
            subscribers: Arc::new(Subscribers::new()),
            connections: Mutex::new(HashMap::new()),
            started: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            scanner: Mutex::new(None),
        }))
    }
}

/// Named machines over one store, exposed to an axum application. Build with
/// [`StatechartRegistry::builder`]; mount with [`crate::router`].
pub struct StatechartRegistry {
    pub(crate) store: Arc<dyn StateStore>,
    pub(crate) options: OptionsFactory,
    pub(crate) inbox: Option<Arc<dyn InboxStore>>,
    principal: Option<Arc<dyn PrincipalResolver>>,
    pub(crate) settings: Settings,
    scanner_now: Option<NowFn>,
    pub(crate) regs: Arc<HashMap<String, Arc<Registration>>>,
    observers: RwLock<Vec<ObserverFactory>>,
    subscribers: Arc<Subscribers>,
    connections: Mutex<HashMap<Topic, usize>>,
    started: AtomicBool,
    draining: AtomicBool,
    scanner: Mutex<Option<scanner::ScannerHandle>>,
}

impl fmt::Debug for StatechartRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatechartRegistry")
            .field("machines", &self.machine_names())
            .field("settings", &self.settings)
            .field("inbox", &self.inbox.is_some())
            .field("draining", &self.is_draining())
            .finish_non_exhaustive()
    }
}

fn relock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl StatechartRegistry {
    /// Start building a registry over `store`.
    #[must_use]
    pub fn builder(store: Arc<dyn StateStore>) -> RegistryBuilder {
        RegistryBuilder {
            store,
            options: Arc::new(PersistedOptions::default),
            inbox: None,
            principal: None,
            settings: Settings {
                max_body_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
                body_timeout: DEFAULT_BODY_TIMEOUT,
                heartbeat: Duration::from_secs(15),
                max_connections_per_key: 16,
                drain_timeout: Duration::from_secs(10),
                allowed_origins: HashSet::new(),
                timers: None,
            },
            scanner_now: None,
            regs: HashMap::new(),
        }
    }

    /// Registered machine names, sorted.
    #[must_use]
    pub fn machine_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.regs.keys().cloned().collect();
        names.sort();
        names
    }

    /// The machine registered as `name`.
    #[must_use]
    pub fn machine(&self, name: &str) -> Option<&Machine> {
        self.regs.get(name).map(|r| &r.machine)
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn StateStore> {
        &self.store
    }

    /// Largest accepted request body / WebSocket frame.
    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        self.settings.max_body_bytes
    }

    /// The transition fan-out.
    #[must_use]
    pub fn subscribers(&self) -> &Arc<Subscribers> {
        &self.subscribers
    }

    pub(crate) fn reg(&self, name: &str) -> Result<&Arc<Registration>, ApiError> {
        self.regs.get(name).ok_or_else(|| ApiError::UnknownMachine(name.to_owned()))
    }

    /// The store key an instance lives under: `"<name>.<key>"`.
    ///
    /// # Errors
    /// [`ApiError::UnknownMachine`]; [`ApiError::Store`] with `InvalidKey` (400) for an empty
    /// key or a store key over the length limit.
    pub fn store_key(&self, name: &str, key: &str) -> Result<String, ApiError> {
        self.reg(name)?;
        if key.is_empty() {
            return Err(
                StoreError::InvalidKey("Instance key must be a non-empty str.".into()).into()
            );
        }
        let store_key = format!("{name}{KEY_SEP}{key}");
        xstate_persistence::validate_key(&store_key)?;
        Ok(store_key)
    }

    /// The caller resolved by the registry's [`PrincipalResolver`] (`None` without one).
    #[must_use]
    pub fn principal_of(&self, parts: &Parts) -> Option<String> {
        self.principal.as_ref().and_then(|p| p.principal(parts))
    }

    /// Run the machine's authorizer.
    ///
    /// # Errors
    /// [`ApiError::Forbidden`] when refused; [`ApiError::UnknownMachine`].
    pub fn authorize(
        &self,
        principal: Option<&str>,
        name: &str,
        key: &str,
        event: Option<&str>,
    ) -> Result<(), ApiError> {
        let request = AuthRequest { principal, machine: name, key, event };
        self.reg(name)?.options.authorize.authorize(&request).map_err(|_| ApiError::Forbidden)
    }

    /// Attach an observer to every interpreter built from now on (Python appending to
    /// `registry.plugins`).
    pub fn add_observer_factory(&self, factory: ObserverFactory) {
        self.observers.write().unwrap_or_else(PoisonError::into_inner).push(factory);
    }

    pub(crate) fn observers_for(&self, store_key: &str) -> Vec<Arc<dyn Observer<Value>>> {
        let factories = self.observers.read().unwrap_or_else(PoisonError::into_inner);
        factories.iter().map(|f| f(store_key)).collect()
    }

    /// `true` once [`shutdown`](Self::shutdown) began.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// `true` between [`start`](Self::start) and [`shutdown`](Self::shutdown).
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// X0.7: no `Origin` (non-browser), same-origin (against `Host`), or allow-listed.
    #[must_use]
    pub fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN) else { return true };
        let Ok(origin) = origin.to_str() else { return false };
        if self.settings.allowed_origins.contains(origin) {
            return true;
        }
        let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
            return false;
        };
        let bare = origin.split_once("://").map_or(origin, |(_, rest)| rest);
        !host.is_empty() && bare.trim_end_matches('/') == host
    }

    /// Reserve a stream slot for an instance; `None` when the per-key limit is reached or the
    /// registry is draining. The slot is released when the returned guard drops.
    #[must_use]
    pub fn try_open_connection(self: &Arc<Self>, name: &str, key: &str) -> Option<ConnectionSlot> {
        let topic = (name.to_owned(), key.to_owned());
        let mut conns = relock(&self.connections);
        let n = conns.get(&topic).copied().unwrap_or(0);
        if n >= self.settings.max_connections_per_key || self.is_draining() {
            return None;
        }
        conns.insert(topic.clone(), n + 1);
        Some(ConnectionSlot { registry: Arc::clone(self), topic })
    }

    fn close_connection(&self, topic: &Topic) {
        let mut conns = relock(&self.connections);
        match conns.get(topic).copied() {
            Some(n) if n > 1 => {
                conns.insert(topic.clone(), n - 1);
            }
            _ => {
                conns.remove(topic);
            }
        }
    }

    /// Open stream connections of one instance, or of every instance when `topic` is `None`.
    #[must_use]
    pub fn connections(&self, topic: Option<(&str, &str)>) -> usize {
        let conns = relock(&self.connections);
        match topic {
            None => conns.values().sum(),
            Some((name, key)) => {
                conns.get(&(name.to_owned(), key.to_owned())).copied().unwrap_or(0)
            }
        }
    }
}

/// A reserved stream slot (SSE or WebSocket); released on drop.
pub struct ConnectionSlot {
    registry: Arc<StatechartRegistry>,
    topic: Topic,
}

impl fmt::Debug for ConnectionSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionSlot").field("topic", &self.topic).finish_non_exhaustive()
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.registry.close_connection(&self.topic);
    }
}
