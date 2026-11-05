//! Locking strategies and [`persisted`]: make the safe load → act → save pattern the easy one.
//!
//! Two requests transitioning the same entity at once is a lost update or a double side effect.
//! Three mechanisms sit behind one [`LockStrategy`] interface:
//!
//! | Strategy | Behaviour |
//! |----------|-----------|
//! | [`OptimisticLock`] | load → act → save with `expected_version`; reload and retry on [`StoreError::Conflict`] |
//! | [`PessimisticLock`] | hold [`StateStore::lock`] for the whole cycle; one attempt |
//! | [`NoLock`] | load → act → save unconditionally (last writer wins; single-writer only) |
//!
//! # Guarantees
//!
//! * **Actions may run more than once under optimistic retry.** [`LockStrategy::run`] (and
//!   [`persisted_retry`]) re-execute the whole cycle after a conflict, so the body and every
//!   action it triggers can run up to `retries + 1` times per logical send. Put side effects in
//!   services or an outbox, or use [`Session::after_commit`], which only runs after the save
//!   that won.
//! * [`persisted`] runs the body **once**. Under [`OptimisticLock`] a concurrent writer makes it
//!   return [`StoreError::Conflict`]; the caller decides whether to retry.
//! * [`PessimisticLock`] still saves with `expected_version` as a fence, so on a store whose lock
//!   can expire an expired lock yields a conflict, never a lost update.
//! * Each call builds its **own** interpreter; interpreters are never shared between threads.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use xstate_core::clock::{Clock, RealClock};
use xstate_core::snapshot::Snapshot;
use xstate_core::{
    Context, Error as EngineError, Machine, MachineLogic, Observer, RestartTimers, RestoreOptions,
    Status, SyncInterpreter,
};

use crate::store::{LockGuard, Result, SaveOptions, StateStore, StoreError};

// ------------------------------------------------------------------------------ retry policy

/// How a delay is randomised ("Exponential Backoff And Jitter", Brooker 2015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Jitter {
    /// Pure exponential.
    None,
    /// Uniform in `[0, exp]`.
    #[default]
    Full,
    /// `exp / 2 + uniform[0, exp / 2]`.
    Equal,
    /// `min(cap, uniform[base, previous * 3])`; each delay depends on the previous one.
    Decorrelated,
}

/// Exponential backoff with jitter (Python `RetryPolicy`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts allowed, including the first. [`OptimisticLock`] uses its own `retries`
    /// instead; this is for callers driving their own loop.
    pub max_attempts: u32,
    /// Delay after the first failed attempt, in milliseconds.
    pub base_ms: f64,
    /// Exponential growth per attempt.
    pub factor: f64,
    /// Upper bound on any delay, in milliseconds.
    pub max_ms: f64,
    /// Randomisation mode.
    pub jitter: Jitter,
}

/// Backoff of [`OptimisticLock`]: short and jittered. A conflict means another writer just
/// finished, so a few milliseconds of random wait break the herd without user-visible latency
/// (Python `DEFAULT_BACKOFF = RetryPolicy(6, 2 ms, ×2, 100 ms, "full")`).
pub const DEFAULT_BACKOFF: RetryPolicy =
    RetryPolicy { max_attempts: 6, base_ms: 2.0, factor: 2.0, max_ms: 100.0, jitter: Jitter::Full };

impl Default for RetryPolicy {
    fn default() -> Self {
        DEFAULT_BACKOFF
    }
}

impl RetryPolicy {
    /// The un-jittered, capped delay after `attempt` (1-based; `0` is treated as `1`).
    #[must_use]
    pub fn exponential_ms(&self, attempt: u32) -> f64 {
        let raw = self.base_ms * self.factor.powf(f64::from(attempt.max(1) - 1));
        // `0 * inf` is NaN: the capped answer is the only sensible one.
        let capped = if raw.is_nan() { self.max_ms } else { raw.min(self.max_ms) };
        capped.max(0.0)
    }

    /// Delay before the next try, after attempt number `attempt` (1-based) failed.
    ///
    /// `previous_ms` is only used by [`Jitter::Decorrelated`] (`None` means `base_ms`).
    /// `rng` yields uniform samples in `[0, 1]`; out-of-range or non-finite samples are clamped
    /// (Python raises instead) so a faulty generator can never produce a negative or NaN delay.
    pub fn delay_ms(
        &self,
        attempt: u32,
        previous_ms: Option<f64>,
        rng: &mut dyn FnMut() -> f64,
    ) -> f64 {
        let exp = self.exponential_ms(attempt);
        let mut draw = || {
            let r = rng();
            if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 }
        };
        match self.jitter {
            Jitter::None => exp,
            Jitter::Full => draw() * exp,
            Jitter::Equal => {
                let half = exp / 2.0;
                half + draw() * half
            }
            Jitter::Decorrelated => {
                let lo = self.base_ms.max(0.0);
                let prev = previous_ms.filter(|p| *p >= 0.0).unwrap_or(lo);
                let hi = lo.max((prev * 3.0).min(self.max_ms.max(lo)));
                (lo + draw() * (hi - lo)).min(self.max_ms).max(0.0)
            }
        }
    }
}

// ----------------------------------------------------------------------------- strategies

/// How a [`persisted`] block coordinates with other writers.
pub trait LockStrategy: fmt::Debug + Send + Sync {
    /// Hold whatever this strategy holds for the duration of one cycle: a store lock for
    /// [`PessimisticLock`], nothing (`None`) for the others. The guard releases on drop.
    ///
    /// # Errors
    /// [`StoreError::LockTimeout`] or a backend failure.
    fn acquire(&self, store: &dyn StateStore, key: &str) -> Result<Option<Box<dyn LockGuard>>>;

    /// The `expected_version` to save with, given the version that was loaded; `None` for an
    /// unconditional write. A strategy may refuse by returning an error (the timer scanner's
    /// stale-scan check does).
    ///
    /// # Errors
    /// Strategy-specific refusal, normally [`StoreError::Conflict`].
    fn fence(&self, version: u64) -> Result<Option<u64>>;

    /// Extra attempts [`LockStrategy::run`] makes after a conflict (`0` = fail on the first).
    fn conflict_retries(&self) -> u32 {
        0
    }

    /// Backoff between conflict retries.
    fn backoff(&self) -> RetryPolicy {
        DEFAULT_BACKOFF
    }

    /// The generic load → act → save loop under this strategy: `body` receives the restored (or
    /// freshly started) interpreter, and the snapshot is saved when it returns `Ok`.
    ///
    /// `options.lock` is ignored (`self` is the strategy). **`body` may run more than once**
    /// when `self` retries on conflict; see the [module documentation](self).
    ///
    /// # Errors
    /// Whatever [`persisted`] returns; [`StoreError::Conflict`] once the retry budget is spent.
    fn run<C: Context, R>(
        &self,
        store: &dyn StateStore,
        key: &str,
        machine: &Machine,
        logic: &MachineLogic<C>,
        options: &PersistedOptions<C>,
        mut body: impl FnMut(&mut SyncInterpreter<C>) -> Result<R>,
    ) -> Result<R>
    where
        Self: Sized,
    {
        let target = Target { store, key, machine, logic, options };
        run_retrying(self, &target, &mut |interp, _| body(interp))
    }
}

/// A boxed strategy is a strategy, so `options.lock.run(..)` works.
impl LockStrategy for Box<dyn LockStrategy> {
    fn acquire(&self, store: &dyn StateStore, key: &str) -> Result<Option<Box<dyn LockGuard>>> {
        self.as_ref().acquire(store, key)
    }

    fn fence(&self, version: u64) -> Result<Option<u64>> {
        self.as_ref().fence(version)
    }

    fn conflict_retries(&self) -> u32 {
        self.as_ref().conflict_retries()
    }

    fn backoff(&self) -> RetryPolicy {
        self.as_ref().backoff()
    }
}

/// Optimistic concurrency: save with `expected_version`; on [`StoreError::Conflict`] sleep per
/// `backoff`, reload and re-apply the body, up to `retries` extra times.
///
/// Stateless and thread-safe; one instance may be shared. **Actions may run up to
/// `retries + 1` times** per logical send under [`LockStrategy::run`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptimisticLock {
    /// Extra attempts after the first (`0` = fail on the first conflict).
    pub retries: u32,
    /// Wait between attempts.
    pub backoff: RetryPolicy,
}

impl Default for OptimisticLock {
    /// 5 retries with [`DEFAULT_BACKOFF`].
    fn default() -> Self {
        Self { retries: 5, backoff: DEFAULT_BACKOFF }
    }
}

impl LockStrategy for OptimisticLock {
    fn acquire(&self, _store: &dyn StateStore, _key: &str) -> Result<Option<Box<dyn LockGuard>>> {
        Ok(None) // nothing held; the version is the fence
    }

    fn fence(&self, version: u64) -> Result<Option<u64>> {
        Ok(Some(version))
    }

    fn conflict_retries(&self) -> u32 {
        self.retries
    }

    fn backoff(&self) -> RetryPolicy {
        self.backoff
    }
}

/// Pessimistic concurrency: hold [`StateStore::lock`] for the whole cycle (one attempt).
///
/// The save still carries `expected_version`, so a lock that expired under the holder surfaces
/// as [`StoreError::Conflict`] rather than a lost update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PessimisticLock {
    /// How long to wait for the lock.
    pub timeout: Duration,
}

impl Default for PessimisticLock {
    /// A ten second wait.
    fn default() -> Self {
        Self { timeout: Duration::from_secs(10) }
    }
}

impl LockStrategy for PessimisticLock {
    fn acquire(&self, store: &dyn StateStore, key: &str) -> Result<Option<Box<dyn LockGuard>>> {
        store.lock(key, self.timeout).map(Some)
    }

    fn fence(&self, version: u64) -> Result<Option<u64>> {
        Ok(Some(version))
    }
}

/// Load → act → save with an unconditional write: last writer wins. Only correct with exactly
/// one writer per key; it exists so that choice is explicit rather than the accident of
/// forgetting a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoLock;

impl LockStrategy for NoLock {
    fn acquire(&self, _store: &dyn StateStore, _key: &str) -> Result<Option<Box<dyn LockGuard>>> {
        Ok(None)
    }

    fn fence(&self, _version: u64) -> Result<Option<u64>> {
        Ok(None)
    }
}

// ------------------------------------------------------------------------------- options

/// What to do when the stored `machine_version` differs from the running machine's.
///
/// Nothing is compared when either side is unlabelled (an unlabelled record for a labelled
/// machine only logs one warning per machine); the structural hash still guards.
#[derive(Clone, Default)]
pub enum VersionMismatch {
    /// Refuse with [`StoreError::Engine`] wrapping a snapshot error.
    #[default]
    Error,
    /// Log a warning and restore as-is (the change is known to be compatible).
    Warn,
    /// Upcast the snapshot with this function, then restore the result. Its error aborts the
    /// restore.
    Migrate(Arc<dyn Fn(Snapshot) -> Result<Snapshot> + Send + Sync>),
}

impl fmt::Debug for VersionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("Error"),
            Self::Warn => f.write_str("Warn"),
            Self::Migrate(_) => f.write_str("Migrate(..)"),
        }
    }
}

/// Settings of [`persisted`], [`load_interpreter`] and [`LockStrategy::run`].
///
/// Use struct-update syntax to override a few: `PersistedOptions { lock: Box::new(NoLock),
/// ..PersistedOptions::default() }`.
pub struct PersistedOptions<C: Context> {
    /// Coordination with other writers (default [`OptimisticLock`]).
    pub lock: Box<dyn LockStrategy>,
    /// Clock of the restored interpreter (default a fresh [`RealClock`]).
    pub clock: Arc<dyn Clock>,
    /// Start a fresh machine when the key is unknown (default `true`); otherwise
    /// [`StoreError::NotFound`].
    pub create_if_missing: bool,
    /// Refuse a snapshot whose structural hash differs from the machine's (default `true`).
    pub verify_hash: bool,
    /// The structural hash the caller holds for the machine (record
    /// [`Machine::structure_hash`] when you persist). Checked against the machine itself, so it
    /// also guards a **new** key, and against the stored snapshot's own hash on a restore, where
    /// it forces the drift check on even with `verify_hash == false`. A mismatch is
    /// [`StoreError::Engine`] wrapping a snapshot error.
    pub expected_machine_hash: Option<String>,
    /// How persisted `after` timers are re-armed (default [`RestartTimers::Resume`]).
    pub restart_timers: RestartTimers,
    /// Machine-version policy (default [`VersionMismatch::Error`]).
    pub on_version_mismatch: VersionMismatch,
    /// Observers attached to the interpreter. On a restore they are attached after
    /// `from_snapshot`, so timers fired by [`RestartTimers::FireDue`] are not reported to them.
    pub observers: Vec<Arc<dyn Observer<C>>>,
}

impl<C: Context> Default for PersistedOptions<C> {
    fn default() -> Self {
        Self {
            lock: Box::new(OptimisticLock::default()),
            clock: Arc::new(RealClock::new()),
            create_if_missing: true,
            verify_hash: true,
            expected_machine_hash: None,
            restart_timers: RestartTimers::Resume,
            on_version_mismatch: VersionMismatch::Error,
            observers: Vec::new(),
        }
    }
}

impl<C: Context> fmt::Debug for PersistedOptions<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistedOptions")
            .field("lock", &self.lock)
            .field("clock", &self.clock)
            .field("create_if_missing", &self.create_if_missing)
            .field("verify_hash", &self.verify_hash)
            .field("expected_machine_hash", &self.expected_machine_hash)
            .field("restart_timers", &self.restart_timers)
            .field("on_version_mismatch", &self.on_version_mismatch)
            .field("observers", &self.observers.len())
            .finish()
    }
}

type Callback = Box<dyn FnOnce() -> Result<()>>;

/// Handle passed to the body of [`persisted`]; collects work to do once the state is durable.
#[derive(Default)]
pub struct Session {
    callbacks: Vec<Callback>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("callbacks", &self.callbacks.len()).finish()
    }
}

impl Session {
    /// Run `callback` after the enclosing block has **saved and released its lock**, for
    /// non-transactional side effects (a broker publish) that must not happen for a state that
    /// never became durable. Callbacks run in registration order; they are dropped when the body
    /// or the save fails, and (under [`LockStrategy::run`]) when the attempt is retried.
    ///
    /// Every callback runs even if an earlier one fails; failures are logged and never undo or
    /// fail the save.
    pub fn after_commit(&mut self, callback: impl FnOnce() -> Result<()> + 'static) {
        self.callbacks.push(Box::new(callback));
    }

    fn run_callbacks(self) {
        for callback in self.callbacks {
            if let Err(error) = callback() {
                tracing::error!(target: "xstate::persistence", %error, "after_commit callback failed");
            }
        }
    }
}

// ------------------------------------------------------------------------- load and save

/// Everything one cycle needs, bundled to keep signatures short.
struct Target<'a, C: Context> {
    store: &'a dyn StateStore,
    key: &'a str,
    machine: &'a Machine,
    logic: &'a MachineLogic<C>,
    options: &'a PersistedOptions<C>,
}

/// Load `key` into a **started** (or restored) interpreter, creating one when the key is
/// missing and `options.create_if_missing` allows it. Returns the interpreter and the loaded
/// record version (`0` for a new key), the value to pass as `expected_version` when saving.
///
/// `clock` drives the interpreter (`options.clock` is not consulted). The version policy,
/// `verify_hash`, `expected_machine_hash`, `restart_timers` and `observers` of `options` apply
/// (the hash expectation on a new key too: it is checked against the machine itself); `options.lock` does not
/// (no lock is taken here).
///
/// # Errors
/// [`StoreError::NotFound`] for a missing key with `create_if_missing = false`; store errors;
/// [`StoreError::Engine`] for a corrupt, drifted or version-mismatched snapshot, or a machine
/// whose initial entry fails.
pub fn load_interpreter<C: Context>(
    store: &dyn StateStore,
    key: &str,
    machine: &Machine,
    logic: &MachineLogic<C>,
    clock: Arc<dyn Clock>,
    options: &PersistedOptions<C>,
) -> Result<(SyncInterpreter<C>, u64)> {
    let Some(record) = store.load(key)? else {
        if !options.create_if_missing {
            return Err(StoreError::NotFound(key.to_owned()));
        }
        check_expected_hash(
            machine,
            options.expected_machine_hash.as_deref(),
            machine.structure_hash(),
        )?;
        let mut interp = SyncInterpreter::with_clock(machine.clone(), logic.clone(), clock);
        for observer in &options.observers {
            interp.observe(Arc::clone(observer));
        }
        interp.start()?;
        refuse_failed_start(&interp)?;
        return Ok((interp, 0));
    };
    let snapshot = Snapshot::from_json(&record.snapshot)?;
    let expected = options.expected_machine_hash.as_deref();
    check_expected_hash(machine, expected, &snapshot.machine_hash)?;
    let snapshot = apply_version_policy(snapshot, machine, &options.on_version_mismatch)?;
    let restore = RestoreOptions {
        verify_hash: Some(options.verify_hash || expected.is_some()),
        restart_timers: options.restart_timers,
    };
    let mut interp =
        SyncInterpreter::from_snapshot(machine.clone(), logic.clone(), &snapshot, clock, &restore)?;
    for observer in &options.observers {
        interp.observe(Arc::clone(observer));
    }
    Ok((interp, record.version))
}

/// Refuse when the caller's `expected` fingerprint is not `actual` (the machine's own hash for a
/// new key, the payload's for a restore).
fn check_expected_hash(machine: &Machine, expected: Option<&str>, actual: &str) -> Result<()> {
    match expected {
        Some(want) if want != actual => Err(EngineError::Snapshot(format!(
            "caller's expected_machine_hash {want:?} does not match machine '{}' ({actual})",
            machine.id()
        ))
        .into()),
        _ => Ok(()),
    }
}

/// Persist `interp`'s snapshot under `key` and return the new version.
///
/// The record's `machine_version` is the machine's label (empty when it has none) and its
/// deadlines index the snapshot's durable `after` timers. `expected_version` follows
/// [`SaveOptions::expected_version`] (`None` = unconditional, `Some(0)` = the key must not
/// exist).
///
/// # Errors
/// [`StoreError::Conflict`] when `expected_version` does not match; store errors;
/// [`StoreError::Engine`] when the interpreter cannot be snapshotted (not started).
pub fn save_interpreter<C: Context>(
    store: &dyn StateStore,
    key: &str,
    interp: &SyncInterpreter<C>,
    expected_version: Option<u64>,
) -> Result<u64> {
    let snapshot = interp.snapshot()?;
    let json = snapshot.to_json()?;
    let options = SaveOptions {
        expected_version,
        machine_version: interp.machine().version().unwrap_or(""),
        deadlines: &snapshot.deadlines,
    };
    store.save(key, &json, &options)
}

/// A machine that `start()` itself put into `error` is not a committed step: saving it would
/// overwrite the stored state with a terminal snapshot that could never run again.
fn refuse_failed_start<C: Context>(interp: &SyncInterpreter<C>) -> Result<()> {
    if interp.status() != Status::Error {
        return Ok(());
    }
    let reason = interp.state().and_then(|s| s.error()).unwrap_or("unknown error");
    Err(EngineError::logic(format!("machine failed to start: {reason}")).into())
}

fn apply_version_policy(
    snapshot: Snapshot,
    machine: &Machine,
    policy: &VersionMismatch,
) -> Result<Snapshot> {
    let Some(expected) = machine.version() else { return Ok(snapshot) };
    let Some(found) = snapshot.machine_version.clone() else {
        warn_unlabelled_once(machine.id(), expected);
        return Ok(snapshot);
    };
    if found == expected {
        return Ok(snapshot);
    }
    match policy {
        VersionMismatch::Warn => {
            tracing::warn!(
                target: "xstate::persistence",
                machine = machine.id(), found = %found, running = expected,
                "snapshot is from another machine version; restoring as-is"
            );
            Ok(snapshot)
        }
        VersionMismatch::Migrate(migrate) => {
            tracing::info!(
                target: "xstate::persistence",
                machine = machine.id(), found = %found, running = expected,
                "migrating snapshot"
            );
            migrate(snapshot)
        }
        VersionMismatch::Error => Err(EngineError::Snapshot(format!(
            "snapshot of machine '{}' was written by version '{found}' but the running machine \
             is version '{expected}'; migrate it or choose the Warn policy if the change is \
             compatible",
            machine.id()
        ))
        .into()),
    }
}

/// Warn once per (machine, version) pair: 10 000 restores of unlabelled records must not log
/// 10 000 warnings.
fn warn_unlabelled_once(machine_id: &str, expected: &str) {
    const MAX_REMEMBERED: usize = 1024;
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let mut seen = SEEN.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner);
    if seen.len() >= MAX_REMEMBERED {
        seen.clear();
    }
    if seen.insert((machine_id.to_owned(), expected.to_owned())) {
        tracing::warn!(
            target: "xstate::persistence",
            machine = machine_id, running = expected,
            "snapshot carries no machine_version; cannot verify the chart revision"
        );
    }
}

// ---------------------------------------------------------------------------- the cycle

type Body<'b, C, R> = &'b mut dyn FnMut(&mut SyncInterpreter<C>, &mut Session) -> Result<R>;

/// One acquire → load → body → fence → save → stop pass. The lock (if any) is held until the
/// end of this function, so callers run `after_commit` callbacks after it is released.
fn cycle<C: Context, R>(
    target: &Target<'_, C>,
    strategy: &dyn LockStrategy,
    session: &mut Session,
    body: Body<'_, C, R>,
) -> Result<R> {
    let _lock = strategy.acquire(target.store, target.key)?;
    let (mut interp, version) = load_interpreter(
        target.store,
        target.key,
        target.machine,
        target.logic,
        Arc::clone(&target.options.clock),
        target.options,
    )?;
    let outcome = body(&mut interp, session).and_then(|result| {
        let expected = strategy.fence(version)?;
        save_interpreter(target.store, target.key, &interp, expected)?;
        Ok(result)
    });
    interp.stop();
    outcome
}

fn run_retrying<C: Context, R>(
    strategy: &dyn LockStrategy,
    target: &Target<'_, C>,
    body: Body<'_, C, R>,
) -> Result<R> {
    let retries = strategy.conflict_retries();
    let backoff = strategy.backoff();
    let mut previous_ms = None;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let mut session = Session::default();
        match cycle(target, strategy, &mut session, body) {
            Ok(result) => {
                session.run_callbacks();
                return Ok(result);
            }
            Err(StoreError::Conflict { .. }) if attempt <= retries => {
                let ms = backoff.delay_ms(attempt, previous_ms, &mut rand::random::<f64>);
                previous_ms = Some(ms);
                std::thread::sleep(Duration::try_from_secs_f64(ms / 1000.0).unwrap_or_default());
            }
            Err(error) => return Err(error),
        }
    }
}

/// The Python `persisted()` context manager as a closure: create → act → persist → discard.
///
/// 1. Take `options.lock`'s lock (if any).
/// 2. [`load_interpreter`]: restore the stored snapshot, or start a fresh machine when the key is
///    missing and `create_if_missing` is set (else [`StoreError::NotFound`]).
/// 3. Run `body` **once**. If it returns `Err`, nothing is written and the error is returned.
/// 4. On `Ok`, save `interp.snapshot()` with `expected_version` from the strategy's
///    [`fence`](LockStrategy::fence) (the loaded version, or `0` for a new key). A clean exit
///    always saves, so the version advances even if the body sent nothing.
/// 5. Release the lock, then run the callbacks the body registered with
///    [`Session::after_commit`].
///
/// Under [`OptimisticLock`] a concurrent writer makes the save fail with
/// [`StoreError::Conflict`]; use [`persisted_retry`] or [`LockStrategy::run`] for automatic
/// retries (the body then runs more than once).
///
/// # Errors
/// Everything [`load_interpreter`] and [`save_interpreter`] return, lock timeouts, and the
/// body's own error.
pub fn persisted<C: Context, R>(
    store: &dyn StateStore,
    key: &str,
    machine: &Machine,
    logic: &MachineLogic<C>,
    options: &PersistedOptions<C>,
    mut body: impl FnMut(&mut SyncInterpreter<C>, &mut Session) -> Result<R>,
) -> Result<R> {
    let target = Target { store, key, machine, logic, options };
    let mut session = Session::default();
    let result = cycle(&target, options.lock.as_ref(), &mut session, &mut body)?;
    session.run_callbacks();
    Ok(result)
}

/// [`persisted`] with `options.lock`'s retry loop (Python `persisted_retry`): the body may run
/// up to `retries + 1` times under [`OptimisticLock`]; `after_commit` callbacks of failed
/// attempts are discarded.
///
/// # Errors
/// As [`persisted`]; [`StoreError::Conflict`] once the retry budget is spent.
pub fn persisted_retry<C: Context, R>(
    store: &dyn StateStore,
    key: &str,
    machine: &Machine,
    logic: &MachineLogic<C>,
    options: &PersistedOptions<C>,
    mut body: impl FnMut(&mut SyncInterpreter<C>, &mut Session) -> Result<R>,
) -> Result<R> {
    let target = Target { store, key, machine, logic, options };
    run_retrying(options.lock.as_ref(), &target, &mut body)
}
