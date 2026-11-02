//! [`CircuitBreaker`]: Nygard's pattern as a statechart.
//!
//! The breaker *is* a three-state chart (see [`CIRCUIT_BREAKER_CONFIG`]):
//!
//! ```text
//! closed    --FAILURE[threshold reached]--> open
//! open      --after(cooldown)-------------> half_open
//! half_open --SUCCESS--> closed   |   half_open --FAILURE--> open
//! ```
//!
//! It runs on a `SyncInterpreter` (thread-free timers; a `tick` drains the cooldown) behind
//! **one lock**. Because the chart owns the state, the usual half-open race -- two callers who
//! both think they are the probe -- is solved where it belongs: [`CircuitBreaker::admit`] is a
//! guarded `PROBE` transition inside the lock, so exactly `half_open_max_calls` probes get
//! through per half-open window however many threads hammer it.
//!
//! Observers attached with [`CircuitBreakerBuilder::observer`] are notified **after** the lock is
//! released (their callbacks are recorded under it and replayed), so an observer may call
//! back into the breaker. The probe slot of a half-open call that ends without counting
//! ([`Permit::ignore`], a non-counting error in [`CircuitBreaker::call_filtered`]) is given
//! back.
//!
//! The lock also wraps `tick`: the cooldown is an `after`, which fires only when someone pumps
//! the clock, so [`CircuitBreaker::state`] ticks first (otherwise it would report a stale
//! `open` after the cooldown elapsed) and that tick must not race a concurrent call.
//!
//! ```
//! use std::sync::Arc;
//! use xstate_core::SimulatedClock;
//! use xstate_patterns::{CallError, CircuitBreaker, CircuitState};
//!
//! let clock = SimulatedClock::new();
//! let breaker = CircuitBreaker::builder()
//!     .failure_threshold(2)
//!     .cooldown_ms(1_000.0)
//!     .clock(Arc::new(clock.clone()))
//!     .build()?;
//!
//! for _ in 0..2 {
//!     let _ = breaker.call(|| Err::<(), _>("boom"));
//! }
//! assert_eq!(breaker.state(), CircuitState::Open);
//! assert!(matches!(breaker.call(|| Ok::<_, &str>(1)), Err(CallError::Rejected(_))));
//!
//! clock.increment(1_001.0);
//! assert_eq!(breaker.state(), CircuitState::HalfOpen);
//! assert_eq!(breaker.call(|| Ok::<_, &str>(1)).ok(), Some(1));
//! assert_eq!(breaker.state(), CircuitState::Closed);
//! # Ok::<(), xstate_patterns::CircuitBreakerError>(())
//! ```

mod config;
mod notify;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use xstate_core::SyncInterpreter;
use xstate_core::{Clock, DynContext, Machine, MachineLogic, Observer, RealClock, Status};

pub use config::{CIRCUIT_BREAKER_CONFIG, circuit_breaker_config, circuit_breaker_logic};
use notify::{Collector, Queue};

use crate::ctx::counter;

/// The three breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CircuitState {
    /// Calls pass through; consecutive failures are counted.
    Closed,
    /// Calls fail fast while the cooldown runs.
    Open,
    /// A bounded number of probe calls is admitted.
    HalfOpen,
}

impl CircuitState {
    /// The state's name in the chart (`closed`, `open`, `half_open`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }

    fn from_leaf(leaf: &str) -> Self {
        match leaf {
            "open" => Self::Open,
            "half_open" => Self::HalfOpen,
            _ => Self::Closed,
        }
    }
}

impl fmt::Display for CircuitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The circuit is open (or half-open with no probe slot left); the target was **not** invoked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Circuit '{breaker}' is {state}; call rejected without invoking the target.")]
#[non_exhaustive]
pub struct CircuitOpenError {
    /// The breaker's name.
    pub breaker: String,
    /// The state it was in when the call was rejected.
    pub state: CircuitState,
}

/// Why [`CircuitBreaker::admit`] refused a call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AdmitError {
    /// Open, or half-open without a free probe slot.
    #[error(transparent)]
    Open(#[from] CircuitOpenError),
    /// After [`CircuitBreaker::close`] the interpreter is stopped and every event would be
    /// dropped; running the target unprotected would be worse than refusing.
    #[error("Circuit '{breaker}' is closed (stopped); call rejected without invoking the target.")]
    Stopped {
        /// The breaker's name.
        breaker: String,
    },
}

/// Outcome of [`CircuitBreaker::call`] when the call did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CallError<E> {
    /// The breaker refused the call; the closure was not run.
    #[error(transparent)]
    Rejected(AdmitError),
    /// The closure ran and returned this error.
    #[error("{0}")]
    Failed(E),
}

/// Why a [`CircuitBreaker`] could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CircuitBreakerError {
    /// A parameter is out of range.
    #[error("invalid circuit breaker: {0}")]
    Invalid(&'static str),
    /// The engine rejected the chart or failed to start it.
    #[error(transparent)]
    Engine(#[from] xstate_core::Error),
}

/// `(generation, opened_count in the interpreter, state)`: the admission window an outcome
/// must be recorded in.
type Window = (u64, u64, CircuitState);

struct Inner {
    interp: SyncInterpreter<DynContext>,
    /// Bumped by `reset()`; part of the admission window.
    generation: u64,
    /// Trips counted by interpreters replaced in `reset()` (the lifetime counter must not
    /// restart, and a running interpreter's context cannot be edited from outside).
    base_opened: u64,
}

impl Inner {
    fn state(&self) -> CircuitState {
        self.interp
            .state_ids()
            .first()
            .and_then(|id| id.rsplit('.').next())
            .map_or(CircuitState::Closed, CircuitState::from_leaf)
    }

    fn context_counter(&self, key: &str) -> u64 {
        self.interp.context().map_or(0, |ctx| counter(ctx, key))
    }

    fn window(&self) -> Window {
        (self.generation, self.context_counter("opened_count"), self.state())
    }

    fn tick(&mut self) {
        if let Err(err) = self.interp.tick() {
            tracing::warn!(target: "xstate_patterns", %err, "circuit breaker tick failed");
        }
    }

    fn send(&mut self, event: &str) -> Option<xstate_core::Receipt> {
        match self.interp.send(event) {
            Ok(receipt) => Some(receipt),
            Err(err) => {
                tracing::warn!(target: "xstate_patterns", %err, event, "circuit breaker send failed");
                None
            }
        }
    }
}

/// Builder for [`CircuitBreaker`] (defaults: threshold 5, cooldown 30 s, one probe).
pub struct CircuitBreakerBuilder {
    failure_threshold: u32,
    cooldown_ms: f64,
    half_open_max_calls: u32,
    clock: Option<Arc<dyn Clock>>,
    name: Option<String>,
    observers: Vec<Arc<dyn Observer<DynContext>>>,
}

impl fmt::Debug for CircuitBreakerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerBuilder")
            .field("failure_threshold", &self.failure_threshold)
            .field("cooldown_ms", &self.cooldown_ms)
            .field("half_open_max_calls", &self.half_open_max_calls)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Default for CircuitBreakerBuilder {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown_ms: 30_000.0,
            half_open_max_calls: 1,
            clock: None,
            name: None,
            observers: Vec::new(),
        }
    }
}

impl CircuitBreakerBuilder {
    /// Consecutive failures that open the circuit (must be `>= 1`).
    #[must_use]
    pub const fn failure_threshold(mut self, n: u32) -> Self {
        self.failure_threshold = n;
        self
    }

    /// How long the circuit stays open before admitting probes (finite, `>= 0`).
    #[must_use]
    pub const fn cooldown_ms(mut self, ms: f64) -> Self {
        self.cooldown_ms = ms;
        self
    }

    /// Probes admitted per half-open window (must be `>= 1`).
    #[must_use]
    pub const fn half_open_max_calls(mut self, n: u32) -> Self {
        self.half_open_max_calls = n;
        self
    }

    /// The clock; pass a `SimulatedClock` in tests and advance it past the cooldown to reach
    /// half-open.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Name used in [`CircuitOpenError`] messages and as the machine id.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Attach an observer to the internal interpreter (it sees every trip).
    ///
    /// The breaker records the observer's callbacks while it holds its lock and replays them
    /// right after releasing it, on the thread that caused them, so an observer may call back
    /// into the breaker. Callbacks caused by different threads at once may interleave, and
    /// `on_error` receives an `Error::Logic` carrying the original message.
    #[must_use]
    pub fn observer(mut self, observer: Arc<dyn Observer<DynContext>>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Validate and build.
    ///
    /// # Errors
    /// [`CircuitBreakerError::Invalid`] for a zero threshold or probe count, or a negative /
    /// non-finite cooldown (a negative cooldown half-opens instantly; NaN and infinity leave
    /// the circuit open forever -- silent acceptance would be a bug);
    /// [`CircuitBreakerError::Engine`] if the chart fails to compile or start.
    pub fn build(self) -> Result<CircuitBreaker, CircuitBreakerError> {
        if self.failure_threshold < 1 || self.half_open_max_calls < 1 {
            return Err(CircuitBreakerError::Invalid(
                "failure_threshold and half_open_max_calls must be >= 1",
            ));
        }
        if !self.cooldown_ms.is_finite() || self.cooldown_ms < 0.0 {
            return Err(CircuitBreakerError::Invalid("cooldown_ms must be a finite number >= 0"));
        }
        let mut config = circuit_breaker_config();
        config["context"]["failure_threshold"] = Value::from(self.failure_threshold);
        config["context"]["half_open_max_calls"] = Value::from(self.half_open_max_calls);
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            config["id"] = Value::from(name);
        }
        let machine = Machine::from_value(&config)?;
        let clock = self.clock.unwrap_or_else(|| Arc::new(RealClock::new()));
        let released = Arc::new(AtomicU64::new(0));
        let logic = config::breaker_logic(self.cooldown_ms, &released);
        let queue = Queue::default();
        let collector =
            (!self.observers.is_empty()).then(|| Arc::new(Collector::new(Arc::clone(&queue))));
        let interp = start_interpreter(&machine, &logic, &clock, collector.as_ref())?;
        let breaker = CircuitBreaker {
            name: machine.id().to_owned(),
            machine,
            logic,
            clock,
            observers: self.observers,
            collector,
            queue,
            released,
            inner: Mutex::new(Inner { interp, generation: 0, base_opened: 0 }),
        };
        breaker.flush();
        Ok(breaker)
    }
}

fn start_interpreter(
    machine: &Machine,
    logic: &MachineLogic<DynContext>,
    clock: &Arc<dyn Clock>,
    collector: Option<&Arc<Collector>>,
) -> xstate_core::Result<SyncInterpreter<DynContext>> {
    let mut interp = SyncInterpreter::with_clock(machine.clone(), logic.clone(), Arc::clone(clock));
    if let Some(collector) = collector {
        interp.observe(Arc::clone(collector) as Arc<dyn Observer<DynContext>>);
    }
    interp.start()?;
    Ok(interp)
}

/// A thread-safe circuit breaker driven by [`CIRCUIT_BREAKER_CONFIG`].
///
/// [`call`](Self::call) runs a fallible closure if the breaker admits it, records the outcome
/// and returns the closure's result (or a [`CallError`]). Share one breaker between threads
/// behind an `Arc`.
pub struct CircuitBreaker {
    name: String,
    machine: Machine,
    logic: MachineLogic<DynContext>,
    clock: Arc<dyn Clock>,
    observers: Vec<Arc<dyn Observer<DynContext>>>,
    /// Records the interpreter's observer callbacks under the lock (`None` without observers).
    collector: Option<Arc<Collector>>,
    queue: Queue,
    /// Probe slots given back in the current half-open window; read by the `probeAvailable`
    /// guard (see `config::breaker_logic`).
    released: Arc<AtomicU64>,
    inner: Mutex<Inner>,
}

impl fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreaker").field("name", &self.name).finish_non_exhaustive()
    }
}

impl CircuitBreaker {
    /// A builder with the defaults (threshold 5, cooldown 30 s, one probe, real clock).
    #[must_use]
    pub fn builder() -> CircuitBreakerBuilder {
        CircuitBreakerBuilder::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `f` under the lock, then (lock released) replay the observer callbacks it caused.
    fn locked<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let result = f(&mut self.lock());
        self.flush();
        result
    }

    /// Deliver the recorded observer callbacks. Never call with the lock held.
    fn flush(&self) {
        if self.collector.is_none() {
            return;
        }
        let notices = notify::drain(&self.queue);
        notify::deliver(&self.machine, &self.observers, &notices);
    }

    /// The breaker's name (the machine id).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current state. Ticks the clock first so an elapsed cooldown is reflected.
    #[must_use]
    pub fn state(&self) -> CircuitState {
        self.locked(|inner| {
            inner.tick();
            inner.state()
        })
    }

    /// Consecutive failures counted in the current closed window.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.lock().context_counter("failures")
    }

    /// How many times the circuit has tripped open, over the breaker's lifetime.
    #[must_use]
    pub fn opened_count(&self) -> u64 {
        let inner = self.lock();
        inner.base_opened + inner.context_counter("opened_count")
    }

    /// Run `f` with read access to the underlying interpreter (snapshots, diagnostics). The
    /// breaker's lock is held for the duration, so `f` must not call back into this breaker
    /// (that would deadlock).
    pub fn with_interpreter<R>(&self, f: impl FnOnce(&SyncInterpreter<DynContext>) -> R) -> R {
        f(&self.lock().interp)
    }

    /// Decide, under the lock, whether one call may proceed. The returned [`Permit`] must be
    /// settled with [`record_success`](Permit::record_success) /
    /// [`record_failure`](Permit::record_failure) (dropping it records a failure).
    ///
    /// # Errors
    /// [`AdmitError::Open`] when open or out of probe slots, [`AdmitError::Stopped`] after
    /// [`close`](Self::close).
    pub fn admit(&self) -> Result<Permit<'_>, AdmitError> {
        let window = self.locked(|inner| self.admit_locked(inner))?;
        Ok(Permit { breaker: self, window, settled: false })
    }

    fn admit_locked(&self, inner: &mut Inner) -> Result<Window, AdmitError> {
        if inner.interp.status() != Status::Running {
            return Err(AdmitError::Stopped { breaker: self.name.clone() });
        }
        inner.tick();
        let state = inner.state();
        let admitted = match state {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => took_probe_slot(inner),
            CircuitState::Open => false,
        };
        if !admitted {
            return Err(CircuitOpenError { breaker: self.name.clone(), state }.into());
        }
        Ok(inner.window())
    }

    /// An outcome only counts in the window that admitted it. A slow call admitted while
    /// `closed` whose result lands after the breaker opened and half-opened must not close the
    /// circuit without any probe (nor a late failure re-open it): late results from an earlier
    /// window -- or from before [`reset`](Self::reset) -- are ignored.
    fn record(&self, ok: bool, window: Window) {
        self.locked(|inner| {
            if window == inner.window() {
                inner.send(if ok { "SUCCESS" } else { "FAILURE" });
            }
        });
    }

    /// Give back the half-open probe slot of an outcome that does not count. Only a permit
    /// admitted in the *current* half-open window owns a slot; anything else is a no-op.
    fn release_probe(&self, window: Window) {
        self.locked(|inner| {
            if window.2 == CircuitState::HalfOpen && window == inner.window() {
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    /// Force the circuit closed (operator override). In-flight outcomes become stale.
    pub fn reset(&self) {
        self.locked(|inner| self.reset_locked(inner));
    }

    fn reset_locked(&self, inner: &mut Inner) {
        inner.generation += 1;
        if inner.state() != CircuitState::Open {
            // A SUCCESS closes half_open and re-zeroes the closed window.
            inner.send("SUCCESS");
            return;
        }
        // From `open` the only way out is the cooldown, so start a fresh interpreter.
        let opened = inner.context_counter("opened_count");
        inner.interp.stop();
        match start_interpreter(&self.machine, &self.logic, &self.clock, self.collector.as_ref()) {
            Ok(fresh) => {
                inner.base_opened += opened;
                inner.interp = fresh;
            }
            Err(err) => {
                tracing::error!(target: "xstate_patterns", %err, "circuit breaker reset failed");
            }
        }
    }

    /// Run `f` through the breaker. Every `Err` counts as a failure.
    ///
    /// # Errors
    /// [`CallError::Rejected`] when the breaker refuses (the closure is not run),
    /// [`CallError::Failed`] with the closure's own error.
    pub fn call<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, CallError<E>> {
        self.call_filtered(f, |_| true)
    }

    /// Like [`call`](Self::call), but only errors for which `counts_as_failure` is `true`
    /// touch the breaker; other errors propagate untouched. A panic inside `f` counts as a
    /// failure and then unwinds.
    ///
    /// # Errors
    /// See [`call`](Self::call).
    pub fn call_filtered<T, E>(
        &self,
        f: impl FnOnce() -> Result<T, E>,
        counts_as_failure: impl Fn(&E) -> bool,
    ) -> Result<T, CallError<E>> {
        let permit = self.admit().map_err(CallError::Rejected)?;
        match f() {
            Ok(value) => {
                permit.record_success();
                Ok(value)
            }
            Err(err) => {
                if counts_as_failure(&err) {
                    permit.record_failure();
                } else {
                    permit.ignore();
                }
                Err(CallError::Failed(err))
            }
        }
    }

    /// Stop the internal interpreter; later calls are rejected with [`AdmitError::Stopped`].
    pub fn close(&self) {
        self.locked(|inner| inner.interp.stop());
    }
}

/// Take a half-open probe slot: send `PROBE` and see whether `countProbe` ran.
fn took_probe_slot(inner: &mut Inner) -> bool {
    inner.send("PROBE").is_some_and(|receipt| receipt.actions.iter().any(|a| &**a == "countProbe"))
}

/// Admission of one call. Settle it exactly once; dropping an unsettled permit (for example
/// during a panic) records a failure so a half-open probe slot is never leaked.
#[derive(Debug)]
#[must_use = "an unsettled permit records a failure when dropped"]
pub struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    window: Window,
    settled: bool,
}

impl Permit<'_> {
    /// Record that the admitted call succeeded.
    pub fn record_success(mut self) {
        self.settle(true);
    }

    /// Record that the admitted call failed.
    pub fn record_failure(mut self) {
        self.settle(false);
    }

    /// Settle without recording anything (the call failed in a way the breaker should not
    /// count). A half-open probe slot is given back so another probe can run; otherwise the
    /// circuit would stay half-open forever once the slots are used up. (The Python library
    /// keeps the slot consumed.)
    pub fn ignore(mut self) {
        self.settled = true;
        self.breaker.release_probe(self.window);
    }

    fn settle(&mut self, ok: bool) {
        self.settled = true;
        self.breaker.record(ok, self.window);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(false);
        }
    }
}

/// Wrap `f` so every invocation goes through `breaker` (the analogue of the Python
/// `@circuit_breaker` decorator; keep the `Arc` to inspect the breaker). A call needing several
/// arguments takes them as a tuple.
///
/// ```
/// use std::sync::Arc;
/// use xstate_patterns::{CircuitBreaker, wrap};
///
/// let breaker = Arc::new(CircuitBreaker::builder().failure_threshold(1).build()?);
/// let fetch = wrap(Arc::clone(&breaker), |id: u32| if id == 0 { Err("not found") } else { Ok(id) });
/// assert_eq!(fetch(7).ok(), Some(7));
/// assert!(fetch(0).is_err());
/// assert_eq!(breaker.opened_count(), 1);
/// # Ok::<(), xstate_patterns::CircuitBreakerError>(())
/// ```
pub fn wrap<A, T, E, F>(breaker: Arc<CircuitBreaker>, f: F) -> impl Fn(A) -> Result<T, CallError<E>>
where
    F: Fn(A) -> Result<T, E>,
{
    move |arg| breaker.call(|| f(arg))
}
