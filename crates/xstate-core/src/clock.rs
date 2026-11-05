//! Clocks. The interpreter never reads wall time directly; it asks a [`Clock`], so tests
//! drive timers with a [`SimulatedClock`] and production uses [`RealClock`].

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Source of monotonic time in milliseconds.
pub trait Clock: fmt::Debug + Send + Sync {
    /// Milliseconds since an arbitrary fixed origin.
    fn now_ms(&self) -> f64;

    /// Wall-clock time in epoch seconds, used to stamp durable deadlines. Defaults to the
    /// system clock; simulated clocks report their own time.
    fn wall_now(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    }
}

/// Monotonic wall clock.
#[derive(Debug, Clone)]
pub struct RealClock {
    origin: Instant,
}

impl Default for RealClock {
    fn default() -> Self {
        Self { origin: Instant::now() }
    }
}

impl RealClock {
    /// A clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clock for RealClock {
    fn now_ms(&self) -> f64 {
        self.origin.elapsed().as_secs_f64() * 1000.0
    }
}

/// A clock that only moves when told to. Clones share the same time.
///
/// Advancing the clock does not fire timers by itself; call the interpreter's `tick()` (or
/// use its `advance()` helper, which does both) so that timers are delivered in due order
/// with the clock already at the new time — the same semantics as XState's `SimulatedClock`.
#[derive(Debug, Clone, Default)]
pub struct SimulatedClock {
    now: Arc<Mutex<f64>>,
}

impl SimulatedClock {
    /// A clock at `0`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward by `ms`. NaN and infinity are ignored (logged); see
    /// [`try_increment`](Self::try_increment) to be told.
    pub fn increment(&self, ms: f64) {
        if let Err(err) = self.try_increment(ms) {
            tracing::error!(target: "xstate", %err, "SimulatedClock::increment ignored");
        }
    }

    /// Move time forward by `ms` (negative values do not move it).
    ///
    /// # Errors
    /// [`Error::Logic`](crate::Error::Logic) for NaN or infinity: a clock at infinity would
    /// fire every timer at once and never move again.
    pub fn try_increment(&self, ms: f64) -> crate::Result<()> {
        finite(ms, "increment")?;
        let mut now = self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *now += ms.max(0.0);
        Ok(())
    }

    /// Set the absolute time (never moves backwards). NaN and infinity are ignored (logged);
    /// see [`try_set`](Self::try_set).
    pub fn set(&self, ms: f64) {
        if let Err(err) = self.try_set(ms) {
            tracing::error!(target: "xstate", %err, "SimulatedClock::set ignored");
        }
    }

    /// Set the absolute time (never moves backwards).
    ///
    /// # Errors
    /// [`Error::Logic`](crate::Error::Logic) for NaN or infinity.
    pub fn try_set(&self, ms: f64) -> crate::Result<()> {
        finite(ms, "set")?;
        let mut now = self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if ms > *now {
            *now = ms;
        }
        Ok(())
    }
}

fn finite(ms: f64, what: &str) -> crate::Result<()> {
    if ms.is_finite() {
        Ok(())
    } else {
        Err(crate::Error::logic(format!("SimulatedClock::{what} needs a finite time, got {ms}")))
    }
}

impl Clock for SimulatedClock {
    fn now_ms(&self) -> f64 {
        *self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wall_now(&self) -> f64 {
        self.now_ms() / 1000.0
    }
}
