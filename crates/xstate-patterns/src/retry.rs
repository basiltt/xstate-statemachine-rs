//! [`RetryPolicy`]: exponential backoff with jitter, as machine logic.
//!
//! In a statechart the retry *is* the chart -- `attempting --error--> retrying
//! --after(delay)--> attempting` -- so the only thing a library can usefully add is the delay
//! function and the three trivial pieces of logic around it (can we retry? bump the counter;
//! reset it). [`RetryPolicy::apply`] registers those under predictable names, so this JSON
//! fragment works verbatim and the retry loop shows up in any inspector:
//!
//! ```json
//! "attempting": {
//!   "invoke": { "src": "work",
//!               "onDone":  { "target": "done",     "actions": "retryReset" },
//!               "onError": { "target": "retrying", "actions": "retryBump" } } },
//! "retrying": {
//!   "after": { "retryDelay": [
//!     { "guard": "retryCanRetry", "target": "attempting" },
//!     { "target": "deadLettered" } ] } }
//! ```
//!
//! Jitter formulas follow the AWS Architecture Blog ("Exponential Backoff And Jitter", Brooker
//! 2015). [`JitterMode::Decorrelated`] is deliberately *not* monotone in the attempt number: each
//! delay depends on the previous one, so its only guarantees are the bounds. The random source
//! is injectable ([`RetryPolicy::with_rng`]) so tests are deterministic.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use xstate_core::{ActionScope, DynContext, Error as EngineError, Event, MachineLogic};

use crate::ERRORS_CONTEXT_KEY;
use crate::ctx::{counter, remove_key, set_key};

/// Default prefix of the generated logic names (`retryDelay`, `retryCanRetry`, ...).
pub const DEFAULT_PREFIX: &str = "retry";
/// Default context key holding the number of attempts made so far.
pub const DEFAULT_ATTEMPT_KEY: &str = "attempt";

/// A source of uniform samples in `[0, 1]`.
pub type RngFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// How a delay is randomised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum JitterMode {
    /// Pure exponential.
    None,
    /// Uniform in `[0, exp]`.
    #[default]
    Full,
    /// `exp / 2 + uniform[0, exp / 2]`.
    Equal,
    /// `min(cap, uniform[base, previous * 3])`.
    Decorrelated,
}

/// Why a [`RetryPolicy`] could not be built or a delay could not be computed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RetryError {
    /// A constructor argument is out of range.
    #[error("invalid retry policy: {0}")]
    Invalid(&'static str),
    /// `attempt` is 1-based; `0` was passed.
    #[error("attempt is 1-based")]
    AttemptZero,
    /// The injected random source returned a value outside `[0, 1]` (or NaN). A silent clamp
    /// would hide a broken generator, so this fails loudly.
    #[error("rng() must return a float in [0, 1], got {0}")]
    RngOutOfRange(f64),
}

/// Exponential backoff with jitter, plus the machine logic to drive it.
///
/// `delay_for(attempt)` takes the 1-based number of the attempt that just *failed* and returns
/// how long to wait before the next one.
#[derive(Clone)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_ms: f64,
    factor: f64,
    max_ms: f64,
    jitter: JitterMode,
    rng: RngFn,
}

impl fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("base_ms", &self.base_ms)
            .field("factor", &self.factor)
            .field("max_ms", &self.max_ms)
            .field("jitter", &self.jitter)
            .finish_non_exhaustive()
    }
}

impl Default for RetryPolicy {
    /// 5 attempts, 200 ms base, factor 2, 30 s cap, full jitter.
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_ms: 200.0,
            factor: 2.0,
            max_ms: 30_000.0,
            jitter: JitterMode::Full,
            rng: default_rng(),
        }
    }
}

fn default_rng() -> RngFn {
    Arc::new(rand::random::<f64>)
}

impl RetryPolicy {
    /// Build a validated policy.
    ///
    /// * `max_attempts` -- total attempts allowed, *including* the first. With `3` the machine
    ///   tries, retries twice, and the third failure is final.
    /// * `base_ms` -- delay before the first retry.
    /// * `factor` -- exponential growth per attempt.
    /// * `max_ms` -- upper bound on any delay; `f64::INFINITY` means "no cap".
    ///
    /// # Errors
    /// [`RetryError::Invalid`] for `max_attempts < 1`, negative or non-finite `base_ms`,
    /// non-finite `factor` or `factor < 1`, and negative or NaN `max_ms`. (NaN slips through
    /// naive `< 0` checks and yields NaN delays; infinity yields an unfireable timer.)
    pub fn new(
        max_attempts: u32,
        base_ms: f64,
        factor: f64,
        max_ms: f64,
        jitter: JitterMode,
    ) -> Result<Self, RetryError> {
        if max_attempts < 1 {
            return Err(RetryError::Invalid("max_attempts must be >= 1"));
        }
        if !(base_ms.is_finite() && factor.is_finite()) {
            return Err(RetryError::Invalid("base_ms and factor must be finite"));
        }
        if max_ms.is_nan() {
            return Err(RetryError::Invalid("max_ms must not be NaN (use infinity for no cap)"));
        }
        if base_ms < 0.0 || max_ms < 0.0 {
            return Err(RetryError::Invalid("base_ms and max_ms must be >= 0"));
        }
        if factor < 1.0 {
            return Err(RetryError::Invalid("factor must be >= 1.0"));
        }
        Ok(Self { max_attempts, base_ms, factor, max_ms, jitter, rng: default_rng() })
    }

    /// Replace the random source with a deterministic one (`() -> f64` in `[0, 1]`).
    #[must_use]
    pub fn with_rng(mut self, rng: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.rng = Arc::new(rng);
        self
    }

    /// Total attempts allowed, including the first.
    #[must_use]
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Delay before the first retry, in milliseconds.
    #[must_use]
    pub const fn base_ms(&self) -> f64 {
        self.base_ms
    }

    /// Exponential growth per attempt.
    #[must_use]
    pub const fn factor(&self) -> f64 {
        self.factor
    }

    /// Upper bound on any delay, in milliseconds.
    #[must_use]
    pub const fn max_ms(&self) -> f64 {
        self.max_ms
    }

    /// The jitter mode.
    #[must_use]
    pub const fn jitter(&self) -> JitterMode {
        self.jitter
    }

    /// Whether another attempt is allowed after `attempts_made` attempts have been made.
    #[must_use]
    pub const fn should_retry(&self, attempts_made: u32) -> bool {
        attempts_made < self.max_attempts
    }

    /// The un-jittered, capped delay after `attempt` (1-based).
    ///
    /// # Errors
    /// [`RetryError::AttemptZero`] for `attempt == 0`.
    pub fn exponential_ms(&self, attempt: u32) -> Result<f64, RetryError> {
        if attempt < 1 {
            return Err(RetryError::AttemptZero);
        }
        let raw = self.base_ms * self.factor.powf(f64::from(attempt - 1));
        // `factor^attempt` overflows a float long before the cap matters, and `0 * inf` is NaN;
        // in both cases the capped answer is unambiguous.
        Ok(if raw.is_nan() { self.max_ms } else { raw.min(self.max_ms) })
    }

    /// Delay before the next try, after `attempt` (1-based) failed. For
    /// [`JitterMode::Decorrelated`] the previous delay is taken as `base_ms`; use
    /// [`delay_after`](Self::delay_after) to chain.
    ///
    /// # Errors
    /// [`RetryError::AttemptZero`], or [`RetryError::RngOutOfRange`] when the injected random
    /// source misbehaves.
    pub fn delay_for(&self, attempt: u32) -> Result<f64, RetryError> {
        self.delay_after(attempt, None)
    }

    /// Like [`delay_for`](Self::delay_for) with the previous delay supplied; only
    /// [`JitterMode::Decorrelated`] reads `previous_ms` (`None`, NaN or negative means
    /// `base_ms`).
    ///
    /// # Errors
    /// See [`delay_for`](Self::delay_for).
    pub fn delay_after(&self, attempt: u32, previous_ms: Option<f64>) -> Result<f64, RetryError> {
        let exp = self.exponential_ms(attempt)?;
        Ok(match self.jitter {
            JitterMode::None => exp,
            JitterMode::Full => self.draw()? * exp,
            JitterMode::Equal => {
                let half = exp / 2.0;
                half + self.draw()? * half
            }
            JitterMode::Decorrelated => {
                // sleep = min(cap, random_between(base, previous * 3))
                let lo = self.base_ms;
                let prev = previous_ms.filter(|p| *p >= 0.0).unwrap_or(lo);
                let hi = lo.max((prev * 3.0).min(self.max_ms.max(lo)));
                self.max_ms.min(lo + self.draw()? * (hi - lo))
            }
        })
    }

    /// One random sample, checked to be in `[0, 1]`.
    fn draw(&self) -> Result<f64, RetryError> {
        let r = (self.rng)();
        if (0.0..=1.0).contains(&r) { Ok(r) } else { Err(RetryError::RngOutOfRange(r)) }
    }

    /// A `MachineLogic` with the default names (`retryDelay`, `retryCanRetry`, `retryBump`,
    /// `retryReset`) over the default `attempt` counter.
    #[must_use]
    pub fn logic(&self) -> MachineLogic<DynContext> {
        self.into_logic(DEFAULT_PREFIX, DEFAULT_ATTEMPT_KEY)
    }

    /// A fresh `MachineLogic` holding only this policy's logic. See [`apply`](Self::apply).
    #[must_use]
    pub fn into_logic(&self, prefix: &str, attempt_key: &str) -> MachineLogic<DynContext> {
        self.apply(MachineLogic::dynamic(), prefix, attempt_key)
    }

    /// Register on `logic`: `{prefix}Delay` (named delay), `{prefix}CanRetry` (guard),
    /// `{prefix}Bump` and `{prefix}Reset` (actions), all driven by the counter at
    /// `ctx[attempt_key]` (the number of attempts made so far).
    ///
    /// * `Delay` reads the counter (`1` means "the first attempt failed") and yields
    ///   `delay_for(counter)`. A misbehaving injected rng surfaces as a logic error.
    /// * `CanRetry` is `counter < max_attempts`.
    /// * `Bump` is `counter += 1`; call it on each failure.
    /// * `Reset` zeroes the counter, forgets the decorrelated memory and clears the
    ///   dead-letter error chain under [`ERRORS_CONTEXT_KEY`]; call it on success.
    ///
    /// For [`JitterMode::Decorrelated`] the previous delay lives in `ctx["{attempt_key}_delay_ms"]`
    /// (the Python layout). A delay implementation only sees the context immutably, so the
    /// delay itself cannot write it: it is also remembered inside the returned logic, tagged
    /// with its attempt number so a stale memory from another run is never reused, and the next
    /// `Bump` copies that memory into the context. The delay reads the context key first and
    /// falls back to the memory only when the key is absent. `Reset` clears both.
    ///
    /// The one remaining difference from Python: Python's delay writes the key the moment it is
    /// computed, so a snapshot taken while waiting shows the current delay; here the key trails
    /// by one delay (the first delay reaches the context at the second `Bump`), and a snapshot
    /// taken while waiting shows the previous one. The delays themselves are identical.
    #[must_use]
    pub fn apply(
        &self,
        logic: MachineLogic<DynContext>,
        prefix: &str,
        attempt_key: &str,
    ) -> MachineLogic<DynContext> {
        let memory: Arc<Mutex<Option<(u64, f64)>>> = Arc::new(Mutex::new(None));
        let delay = self.delay_impl(attempt_key, Arc::clone(&memory));
        let max_attempts = u64::from(self.max_attempts);
        let key = attempt_key.to_owned();
        let bump_key = attempt_key.to_owned();
        let reset_key = attempt_key.to_owned();
        let prev_key = previous_key(attempt_key);
        let bump_prev_key = prev_key.clone();
        let bump_memory = Arc::clone(&memory);
        logic
            .delay_fn(format!("{prefix}Delay"), delay)
            .guard(format!("{prefix}CanRetry"), move |ctx, _event, _params| {
                Ok(counter(ctx, &key) < max_attempts)
            })
            .action(format!("{prefix}Bump"), move |scope: &mut ActionScope<'_, DynContext>| {
                let next = counter(scope.context, &bump_key) + 1;
                set_key(scope.context, &bump_key, Value::from(next));
                // Publish the delay the failed attempt waited, for the next delay to chain on.
                let remembered = *bump_memory.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some((attempts, ms)) = remembered
                    && attempts + 1 == next
                {
                    set_key(scope.context, &bump_prev_key, Value::from(ms));
                }
                Ok(())
            })
            .action(format!("{prefix}Reset"), move |scope: &mut ActionScope<'_, DynContext>| {
                set_key(scope.context, &reset_key, Value::from(0));
                remove_key(scope.context, &prev_key);
                remove_key(scope.context, ERRORS_CONTEXT_KEY);
                *memory.lock().unwrap_or_else(PoisonError::into_inner) = None;
                Ok(())
            })
    }

    fn delay_impl(
        &self,
        attempt_key: &str,
        memory: Arc<Mutex<Option<(u64, f64)>>>,
    ) -> impl Fn(&DynContext, &Event) -> xstate_core::Result<f64> + Send + Sync + 'static {
        let policy = self.clone();
        let key = attempt_key.to_owned();
        let prev_key = previous_key(attempt_key);
        move |ctx, _event| {
            let attempts = counter(ctx, &key).max(1);
            let attempt = u32::try_from(attempts).unwrap_or(u32::MAX);
            let decorrelated = policy.jitter == JitterMode::Decorrelated;
            let previous = if decorrelated {
                ctx.get(&prev_key).and_then(Value::as_f64).or_else(|| {
                    let remembered = *memory.lock().unwrap_or_else(PoisonError::into_inner);
                    remembered.filter(|(n, _)| *n + 1 == attempts).map(|(_, ms)| ms)
                })
            } else {
                None
            };
            let ms = policy.delay_after(attempt, previous).map_err(EngineError::logic)?;
            if decorrelated {
                *memory.lock().unwrap_or_else(PoisonError::into_inner) = Some((attempts, ms));
            }
            Ok(ms)
        }
    }
}

fn previous_key(attempt_key: &str) -> String {
    format!("{attempt_key}_delay_ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(jitter: JitterMode, sample: f64) -> RetryPolicy {
        RetryPolicy::new(5, 100.0, 2.0, 1_000.0, jitter)
            .unwrap_or_else(|e| panic!("{e}"))
            .with_rng(move || sample)
    }

    fn delay(p: &RetryPolicy, attempt: u32) -> f64 {
        p.delay_for(attempt).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn none_is_capped_exponential() {
        let p = policy(JitterMode::None, 0.5);
        let got: Vec<f64> = (1..=5).map(|n| delay(&p, n)).collect();
        assert_eq!(got, [100.0, 200.0, 400.0, 800.0, 1_000.0]);
    }

    #[test]
    fn full_and_equal_scale_the_exponential() {
        assert!((delay(&policy(JitterMode::Full, 0.25), 3) - 100.0).abs() < 1e-9);
        assert!((delay(&policy(JitterMode::Equal, 0.25), 3) - 250.0).abs() < 1e-9);
    }

    #[test]
    fn jitter_bounds_hold_at_the_extremes() {
        for sample in [0.0, 1.0] {
            let full = delay(&policy(JitterMode::Full, sample), 2);
            assert!((0.0..=200.0).contains(&full), "full {full}");
            let equal = delay(&policy(JitterMode::Equal, sample), 2);
            assert!((100.0..=200.0).contains(&equal), "equal {equal}");
        }
    }

    #[test]
    fn decorrelated_stays_within_base_and_cap() {
        let p = policy(JitterMode::Decorrelated, 1.0);
        let mut previous = None;
        for attempt in 1..=8 {
            let ms = p.delay_after(attempt, previous).unwrap_or_else(|e| panic!("{e}"));
            assert!((100.0..=1_000.0).contains(&ms), "decorrelated {ms}");
            previous = Some(ms);
        }
    }

    #[test]
    fn overflow_returns_the_cap() {
        let p = RetryPolicy::new(100, 1.0, 1e6, 5_000.0, JitterMode::None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(delay(&p, 60), 5_000.0);
        let zero_base = RetryPolicy::new(100, 0.0, 1e6, 5_000.0, JitterMode::None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(delay(&zero_base, 200), 5_000.0);
    }

    #[test]
    fn infinite_cap_is_allowed() {
        let p = RetryPolicy::new(3, 10.0, 2.0, f64::INFINITY, JitterMode::None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(delay(&p, 4), 80.0);
    }

    #[test]
    fn attempt_zero_is_an_error() {
        assert_eq!(policy(JitterMode::None, 0.0).delay_for(0), Err(RetryError::AttemptZero));
    }

    #[test]
    fn a_misbehaving_rng_fails_loudly() {
        for bad in [f64::NAN, 1.5, -0.2] {
            let p = policy(JitterMode::Full, bad);
            assert!(matches!(p.delay_for(1), Err(RetryError::RngOutOfRange(_))), "{bad}");
        }
    }

    #[test]
    fn constructor_rejects_bad_parameters() {
        let jitter = JitterMode::None;
        assert!(RetryPolicy::new(0, 1.0, 2.0, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, -1.0, 2.0, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, 1.0, 2.0, -10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, 1.0, 0.5, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, f64::NAN, 2.0, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, f64::INFINITY, 2.0, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, 1.0, f64::INFINITY, 10.0, jitter).is_err());
        assert!(RetryPolicy::new(1, 1.0, 2.0, f64::NAN, jitter).is_err());
    }

    #[test]
    fn should_retry_counts_attempts_made() {
        let p = policy(JitterMode::None, 0.0);
        assert!(p.should_retry(0));
        assert!(p.should_retry(4));
        assert!(!p.should_retry(5));
    }

    #[test]
    fn default_policy_matches_python_defaults() {
        let p = RetryPolicy::default();
        assert_eq!(
            (p.max_attempts(), p.base_ms(), p.factor(), p.max_ms()),
            (5, 200.0, 2.0, 30_000.0)
        );
        assert_eq!(p.jitter(), JitterMode::Full);
        let ms = delay(&p, 1);
        assert!((0.0..=200.0).contains(&ms));
    }
}
