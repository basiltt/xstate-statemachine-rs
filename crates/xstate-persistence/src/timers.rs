//! Durable `after` timers: [`DueTimerScanner`] finds persisted deadlines that matured and wakes
//! their machines.
//!
//! Under create → act → persist → discard the interpreter is discarded after every request, so
//! an in-memory `after: 24h` would never fire. The deadline is therefore stored in the snapshot
//! (layout v4 `deadlines`) and indexed by the store; the scanner is the dependency-free driver
//! that finds matured deadlines and wakes their machines under a [`LockStrategy`] with
//! [`RestartTimers::FireDue`]. Schedulers (cron, a job runner) just call
//! [`run_once`](DueTimerScanner::run_once).
//!
//! # Guarantees
//!
//! * A timer fires no *earlier* than its deadline (minus `skew_tolerance_s`) and no *later* than
//!   the next scan after it.
//! * The scanner re-reads the record under the lock and fires only if a deadline is still due; a
//!   machine that moved on between the scan and the wake is skipped. The version it saw is the
//!   fence for the save, so a concurrent writer turns the wake into a stale skip, never a double
//!   fire.
//! * A crash between firing and saving fires again on the next scan (at-least-once): make the
//!   timer's transition idempotent or pair it with the idempotency inbox.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use xstate_core::clock::Clock;
use xstate_core::snapshot::Deadline;
use xstate_core::{Context, Machine, MachineLogic, RestartTimers};

use crate::locking::{LockStrategy, OptimisticLock, PersistedOptions, persisted};
use crate::store::{LockGuard, Result, StateStore, StoreError, StoredSnapshot, wall_now};

/// The fallback scan must see every key to pick the earliest `limit`.
const ALL_KEYS: usize = 1 << 62;
/// A remaining time is clamped to this many milliseconds (about 31 years), like Python's
/// `Deadline.remaining_ms`.
pub const MAX_REMAINING_MS: f64 = 1e15;
/// Longest single sleep inside [`DueTimerScanner::run_until`], so `stop` is noticed promptly.
const STOP_POLL: Duration = Duration::from_millis(25);

// -------------------------------------------------------------------------------- deadlines

/// Helpers on a persisted [`Deadline`].
pub trait DeadlineExt {
    /// Milliseconds left at wall time `now_wall` (epoch seconds), clamped to
    /// `0..=`[`MAX_REMAINING_MS`]: an overdue deadline is `0`, and a huge (but finite)
    /// `due_at_wall` that would overflow to infinity in the multiplication is capped.
    fn remaining_ms(&self, now_wall: f64) -> f64;

    /// `true` when the deadline has matured at `horizon` (epoch seconds, inclusive).
    fn is_due(&self, horizon: f64) -> bool;
}

impl DeadlineExt for Deadline {
    fn remaining_ms(&self, now_wall: f64) -> f64 {
        let remaining = (self.due_at_wall - now_wall) * 1000.0;
        if remaining.is_nan() || remaining <= 0.0 {
            0.0
        } else {
            remaining.min(MAX_REMAINING_MS).round()
        }
    }

    fn is_due(&self, horizon: f64) -> bool {
        self.due_at_wall <= horizon
    }
}

/// The earliest `due_at_wall` among the record's deadlines that matured at `horizon`, or `None`
/// when none did (the record is stale for a scan at that instant).
#[must_use]
pub fn check_record(record: &StoredSnapshot, horizon: f64) -> Option<f64> {
    record.deadlines.iter().filter(|d| d.is_due(horizon)).map(|d| d.due_at_wall).reduce(f64::min)
}

/// A [`Clock`] whose monotonic time runs normally but whose [`wall_now`](Clock::wall_now) is
/// pinned, so a woken interpreter sees the scan instant as "now" and
/// [`RestartTimers::FireDue`] is deterministic (and testable with an injected scan time).
#[derive(Debug, Clone)]
pub struct FixedWallClock {
    origin: Instant,
    wall: f64,
}

impl FixedWallClock {
    /// A clock reporting `wall` (epoch seconds) as the wall time.
    #[must_use]
    pub fn new(wall: f64) -> Self {
        Self { origin: Instant::now(), wall }
    }
}

impl Clock for FixedWallClock {
    fn now_ms(&self) -> f64 {
        self.origin.elapsed().as_secs_f64() * 1000.0
    }

    fn wall_now(&self) -> f64 {
        self.wall
    }
}

/// The scanner's strategy with `acquire` turned into a no-op: the scanner already holds the
/// lock (to re-check the deadline under it), and re-taking a non-re-entrant store lock would
/// time out on itself. The fence is preserved and additionally pinned to the version the scan
/// saw: if the record moved between the re-check and `persisted`'s own load, the wake is stale.
#[derive(Debug)]
struct Held {
    seen_version: u64,
    fence: Option<u64>,
}

impl Held {
    fn new(inner: &dyn LockStrategy, seen_version: u64) -> Result<Self> {
        Ok(Self { seen_version, fence: inner.fence(seen_version)? })
    }
}

impl LockStrategy for Held {
    fn acquire(&self, _store: &dyn StateStore, _key: &str) -> Result<Option<Box<dyn LockGuard>>> {
        Ok(None)
    }

    fn fence(&self, version: u64) -> Result<Option<u64>> {
        if version != self.seen_version {
            return Err(StoreError::Conflict {
                key: "<scanner>".to_owned(),
                expected: self.seen_version,
                actual: Some(version),
            });
        }
        Ok(self.fence)
    }
}

// ------------------------------------------------------------------------------ scan result

/// What one [`scan`](DueTimerScanner::scan) did.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ScanResult {
    /// Keys inspected: index rows returned for an indexed store, records loaded by the
    /// fallback.
    pub scanned: usize,
    /// Keys with at least one matured deadline.
    pub due: usize,
    /// Machines loaded and fired.
    pub woken: usize,
    /// Due keys skipped because the deadline changed under the lock (another worker won or the
    /// machine moved on), the fenced save lost an optimistic race, or the key's lock could not
    /// be taken (also counted in [`locked`](Self::locked)).
    pub skipped_stale: usize,
    /// Keys whose wake failed, with the error; the scan continues past them.
    pub errors: Vec<(String, StoreError)>,
    /// Worst `now - due_at` among fired deadlines: the "how late are we" metric.
    pub max_lag_s: f64,
    /// How many of [`skipped_stale`](Self::skipped_stale) were a lock timeout. A stuck holder
    /// shows as `locked` climbing every scan while `due` does not fall.
    pub locked: usize,
    /// Due keys for which `machine_for_key` returned `None`.
    pub unmapped: usize,
}

/// A copy of the last scan's counters (the scan's own [`ScanResult`] is returned to its caller
/// and holds non-cloneable errors; this is what a health endpoint can keep reading).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct ScanSummary {
    /// See [`ScanResult::scanned`].
    pub scanned: usize,
    /// See [`ScanResult::due`].
    pub due: usize,
    /// See [`ScanResult::woken`].
    pub woken: usize,
    /// See [`ScanResult::skipped_stale`].
    pub skipped_stale: usize,
    /// Number of entries in [`ScanResult::errors`].
    pub errors: usize,
    /// See [`ScanResult::max_lag_s`].
    pub max_lag_s: f64,
    /// See [`ScanResult::locked`].
    pub locked: usize,
    /// See [`ScanResult::unmapped`].
    pub unmapped: usize,
}

impl From<&ScanResult> for ScanSummary {
    fn from(r: &ScanResult) -> Self {
        Self {
            scanned: r.scanned,
            due: r.due,
            woken: r.woken,
            skipped_stale: r.skipped_stale,
            errors: r.errors.len(),
            max_lag_s: r.max_lag_s,
            locked: r.locked,
            unmapped: r.unmapped,
        }
    }
}

enum Outcome {
    Woken { lag_s: f64 },
    Stale,
    Unmapped,
}

// ----------------------------------------------------------------------------------- scanner

/// Which chart (and logic) a key runs; `None` skips the key.
pub type MachineFactory<C> = Arc<dyn Fn(&str) -> Option<(Machine, MachineLogic<C>)> + Send + Sync>;

/// Wall-clock source in epoch seconds.
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Wakes machines whose persisted `after` deadlines have passed.
///
/// All settings are public fields; [`new`](Self::new) fills in the defaults. Invalid values
/// (`skew_tolerance_s` negative or not finite, `limit == 0`) are refused by
/// [`validate`](Self::validate), which every scan and [`run_until_checked`](Self::run_until_checked)
/// calls first.
pub struct DueTimerScanner<C: Context> {
    /// The store holding the snapshots (open your own handle per process: the scanner is
    /// "another process").
    pub store: Arc<dyn StateStore>,
    /// Which chart a key runs; a closure returning one fixed machine for a single-chart store.
    pub machine_for_key: MachineFactory<C>,
    /// Coordination with writers (default [`OptimisticLock`]).
    pub lock: Box<dyn LockStrategy>,
    /// Fire deadlines up to this many seconds in the *future* too, absorbing clock skew
    /// between the writer host and this one (`0` = strict).
    pub skew_tolerance_s: f64,
    /// Only scan keys with this prefix.
    pub prefix: String,
    /// Keys inspected per scan (batching).
    pub limit: usize,
    /// Epoch-seconds source (default the system clock); inject a fixed value in tests.
    pub now: Option<NowFn>,
    last: Mutex<Option<ScanSummary>>,
}

impl<C: Context> fmt::Debug for DueTimerScanner<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DueTimerScanner")
            .field("lock", &self.lock)
            .field("skew_tolerance_s", &self.skew_tolerance_s)
            .field("prefix", &self.prefix)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<C: Context> DueTimerScanner<C> {
    /// A scanner over `store` with the default policy: [`OptimisticLock`], no skew tolerance,
    /// every key, 1000 keys per scan, the system clock.
    #[must_use]
    pub fn new(store: Arc<dyn StateStore>, machine_for_key: MachineFactory<C>) -> Self {
        Self {
            store,
            machine_for_key,
            lock: Box::new(OptimisticLock::default()),
            skew_tolerance_s: 0.0,
            prefix: String::new(),
            limit: 1000,
            now: None,
            last: Mutex::new(None),
        }
    }

    /// Check the settings.
    ///
    /// # Errors
    /// [`StoreError::Other`] when `skew_tolerance_s` is negative or not finite (a negative one
    /// would make the scanner fire late, an infinite one fire everything) or `limit` is `0` (a
    /// scan that can never find anything).
    pub fn validate(&self) -> Result<()> {
        if !(self.skew_tolerance_s.is_finite() && self.skew_tolerance_s >= 0.0) {
            return Err(StoreError::Other(
                "DueTimerScanner: skew_tolerance_s must be finite and >= 0".to_owned(),
            ));
        }
        if self.limit < 1 {
            return Err(StoreError::Other("DueTimerScanner: limit must be >= 1".to_owned()));
        }
        Ok(())
    }

    /// The counters of the most recent [`scan`](Self::scan); `None` before the first one.
    #[must_use]
    pub fn last_result(&self) -> Option<ScanSummary> {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now_wall(&self) -> f64 {
        self.now.as_ref().map_or_else(wall_now, |now| now())
    }

    fn skew(&self) -> f64 {
        self.skew_tolerance_s.max(0.0)
    }

    fn limit(&self) -> usize {
        self.limit.max(1)
    }

    /// Keys with a matured deadline right now (plus the skew tolerance), soonest first, at most
    /// `limit`.
    ///
    /// # Errors
    /// Backend failures while listing or loading.
    pub fn due_keys(&self) -> Result<Vec<String>> {
        self.validate()?;
        Ok(self.find_due(self.now_wall() + self.skew())?.0)
    }

    /// `(due keys, scanned)` at `horizon`. A store with a deadline index answers directly;
    /// one without ([`StoreError::Other`]) is scanned record by record (O(records) loads).
    fn find_due(&self, horizon: f64) -> Result<(Vec<String>, usize)> {
        // The index has no prefix filter: with a prefix, fetch every due row, filter, then cap,
        // or the store's limit would be spent on other prefixes' keys.
        let cap = if self.prefix.is_empty() { self.limit() } else { ALL_KEYS };
        match self.store.due_keys(horizon, cap) {
            Ok(keys) => {
                let keys: Vec<String> = keys
                    .into_iter()
                    .filter(|key| key.starts_with(&self.prefix))
                    .take(self.limit())
                    .collect();
                let scanned = keys.len();
                Ok((keys, scanned))
            }
            Err(StoreError::Other(_)) => self.find_due_by_loading(horizon),
            Err(error) => Err(error),
        }
    }

    /// Fallback for stores without an index. Inspects *every* key and keeps the earliest
    /// `limit`, so a due key sorting late in key order is never starved.
    fn find_due_by_loading(&self, horizon: f64) -> Result<(Vec<String>, usize)> {
        let keys = self.store.list_keys(&self.prefix, ALL_KEYS)?;
        let scanned = keys.len();
        let mut due: Vec<(f64, String)> = Vec::new();
        for key in keys {
            if let Some(earliest) =
                self.store.load(&key)?.and_then(|record| check_record(&record, horizon))
            {
                due.push((earliest, key));
            }
        }
        due.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        due.truncate(self.limit());
        Ok((due.into_iter().map(|(_, key)| key).collect(), scanned))
    }

    /// Scan once at the scanner's clock; wake every machine with a matured deadline.
    ///
    /// # Errors
    /// Only when discovering due keys fails; per-key failures are collected in
    /// [`ScanResult::errors`].
    pub fn scan(&self) -> Result<ScanResult> {
        self.scan_at(self.now_wall())
    }

    /// [`scan`](Self::scan) at an explicit wall time (epoch seconds).
    ///
    /// # Errors
    /// As [`scan`](Self::scan).
    pub fn scan_at(&self, now: f64) -> Result<ScanResult> {
        self.validate()?;
        let horizon = now + self.skew();
        let (keys, scanned) = self.find_due(horizon)?;
        let mut result = ScanResult { scanned, ..ScanResult::default() };
        for key in keys {
            result.due += 1;
            match self.wake(&key, now, horizon) {
                Ok(Outcome::Woken { lag_s }) => {
                    result.woken += 1;
                    result.max_lag_s = result.max_lag_s.max(lag_s);
                }
                Ok(Outcome::Unmapped) => result.unmapped += 1,
                // Two scanners (or a scanner and a request) racing one key is the normal case;
                // the loser's fenced save was refused and nothing it did was committed.
                Ok(Outcome::Stale) | Err(StoreError::Conflict { .. }) => result.skipped_stale += 1,
                // The same race under a pessimistic lock: retried next scan. A lock stuck for
                // good shows in `locked` rather than as one error per scan.
                Err(StoreError::LockTimeout { .. }) => {
                    tracing::debug!(target: "xstate::persistence", %key, "timer scanner: key is locked");
                    result.skipped_stale += 1;
                    result.locked += 1;
                }
                Err(error) => {
                    tracing::error!(target: "xstate::persistence", %key, %error, "timer scanner: waking failed");
                    result.errors.push((key, error));
                }
            }
        }
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(ScanSummary::from(&result));
        if result.woken > 0 {
            tracing::info!(
                target: "xstate::persistence",
                woken = result.woken, due = result.due, max_lag_s = result.max_lag_s,
                "timer scanner woke machines"
            );
        }
        Ok(result)
    }

    /// One scan; returns how many machines were woken.
    ///
    /// # Errors
    /// As [`scan`](Self::scan).
    pub fn run_once(&self) -> Result<usize> {
        Ok(self.scan()?.woken)
    }

    /// [`run_until_checked`](Self::run_until_checked) for callers that cannot return an error:
    /// invalid settings or a zero `interval` are logged and the loop does not start.
    pub fn run_until(&self, stop: &AtomicBool, interval: Duration) {
        if let Err(error) = self.run_until_checked(stop, interval) {
            tracing::error!(target: "xstate::persistence", %error, "timer scanner not started");
        }
    }

    /// Call [`run_once`](Self::run_once) every `interval` until `stop` is set. A failing scan
    /// is logged and the loop continues.
    ///
    /// # Errors
    /// [`StoreError::Other`] for a zero `interval` (it would spin) or invalid settings
    /// ([`validate`](Self::validate)); nothing is scanned in that case.
    pub fn run_until_checked(&self, stop: &AtomicBool, interval: Duration) -> Result<()> {
        if interval.is_zero() {
            return Err(StoreError::Other("DueTimerScanner: interval must be > 0".to_owned()));
        }
        self.validate()?;
        while !stop.load(Ordering::Acquire) {
            if let Err(error) = self.run_once() {
                tracing::error!(target: "xstate::persistence", %error, "timer scanner: scan failed");
            }
            let wake_at = Instant::now() + interval;
            while !stop.load(Ordering::Acquire) {
                let left = wake_at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(STOP_POLL));
            }
        }
        Ok(())
    }

    /// Lock, re-check and fire one key.
    ///
    /// The strategy's lock is taken first (a real lock for pessimistic, nothing for optimistic
    /// where the fence is the versioned save), then the record is re-read and a matured
    /// deadline must still be there: a machine another worker advanced is skipped, never
    /// double-fired.
    fn wake(&self, key: &str, now: f64, horizon: f64) -> Result<Outcome> {
        let Some((machine, logic)) = (self.machine_for_key)(key) else {
            return Ok(Outcome::Unmapped);
        };
        let _lock = self.lock.acquire(self.store.as_ref(), key)?;
        let Some(record) = self.store.load(key)? else { return Ok(Outcome::Stale) };
        let Some(earliest) = check_record(&record, horizon) else { return Ok(Outcome::Stale) };
        let options = PersistedOptions::<C> {
            lock: Box::new(Held::new(self.lock.as_ref(), record.version)?),
            // The woken machine sees the horizon as "now", so a deadline inside the skew
            // window is matured for `FireDue` exactly as it was for the scan.
            clock: Arc::new(FixedWallClock::new(horizon)),
            create_if_missing: false,
            restart_timers: RestartTimers::FireDue,
            ..PersistedOptions::default()
        };
        // `FireDue` fires inside the restore; leaving the block saves.
        persisted(self.store.as_ref(), key, &machine, &logic, &options, |_, _| Ok(()))?;
        Ok(Outcome::Woken { lag_s: (now - earliest).max(0.0) })
    }
}
