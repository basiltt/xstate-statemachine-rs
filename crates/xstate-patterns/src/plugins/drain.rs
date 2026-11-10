//! Drain plugin: coordinates graceful shutdown, in-flight request completion, and clean actor teardown.
//!
//! Real-world challenge in Rust:
//! Graceful shutdown in Tokio microservices is notoriously difficult. When receiving `SIGTERM` or
//! `Ctrl+C`, applications must:
//! 1. Immediately stop accepting *new* ingress requests (reject with HTTP 503 or dropped event).
//! 2. Allow active, in-flight transitions and background tasks to complete undisturbed.
//! 3. Flush transactional outbox messages and durable snapshots.
//! 4. Safely tear down database connection pools and listeners.
//!
//! Developers frequently introduce race conditions where background workers are dropped prematurely
//! or shutdown hangs indefinitely on orphaned channels.
//!
//! [`DrainPlugin`] models the drain lifecycle directly on the statechart with zero runtime overhead,
//! offering RAII permits for in-flight transitions and a synchronization barrier (`wait_drained`)
//! that resolves when all work reaches quiescence.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use xstate_core::{ActiveState, Context, Event, Machine, Observer};

/// Error returned when trying to admit an event into a draining machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("statechart is draining and rejecting new ingress events")]
pub struct DrainingError;

struct DrainState {
    in_flight: usize,
}

struct Inner {
    draining: AtomicBool,
    rejected_events: AtomicU64,
    state: Mutex<DrainState>,
    quiescent: Condvar,
}

/// RAII permit representing one in-flight statechart operation.
///
/// Dropping the permit decrements the in-flight counter and notifies drain waiters
/// if quiescence (0 active operations) is reached.
pub struct DrainPermit {
    inner: Arc<Inner>,
}

impl fmt::Debug for DrainPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrainPermit").finish_non_exhaustive()
    }
}

impl Drop for DrainPermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.in_flight == 0 {
            self.inner.quiescent.notify_all();
        }
    }
}

/// Coordinates graceful shutdown and in-flight request completion.
pub struct DrainPlugin<C: Context = serde_json::Value> {
    inner: Arc<Inner>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> DrainPlugin<C> {
    /// Create a new graceful drain coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                draining: AtomicBool::new(false),
                rejected_events: AtomicU64::new(0),
                state: Mutex::new(DrainState { in_flight: 0 }),
                quiescent: Condvar::new(),
            }),
            _marker: std::marker::PhantomData,
        }
    }

    /// Attempt to acquire an execution permit.
    ///
    /// # Errors
    /// Returns [`DrainingError`] if shutdown has been initiated.
    pub fn acquire(&self) -> Result<DrainPermit, DrainingError> {
        if self.is_draining() {
            self.inner.rejected_events.fetch_add(1, Ordering::Relaxed);
            return Err(DrainingError);
        }

        let mut state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Double-check under lock
        if self.inner.draining.load(Ordering::Relaxed) {
            self.inner.rejected_events.fetch_add(1, Ordering::Relaxed);
            return Err(DrainingError);
        }

        state.in_flight += 1;
        Ok(DrainPermit { inner: Arc::clone(&self.inner) })
    }

    /// Initiate graceful drain. New permit requests will immediately fail.
    pub fn initiate_drain(&self) {
        self.inner.draining.store(true, Ordering::SeqCst);
        let state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.in_flight == 0 {
            self.inner.quiescent.notify_all();
        }
    }

    /// Check if drain mode is active.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Relaxed)
    }

    /// Current number of in-flight operations.
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        let state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.in_flight
    }

    /// Total events rejected due to drain.
    #[must_use]
    pub fn rejected_events(&self) -> u64 {
        self.inner.rejected_events.load(Ordering::Relaxed)
    }

    /// Wait synchronously or inside a blocking thread until all in-flight work settles or timeout occurs.
    ///
    /// Returns `true` if fully drained (0 active tasks), or `false` if the timeout elapsed.
    pub fn wait_drained(&self, timeout: Duration) -> bool {
        let mut state = self.inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        let start = std::time::Instant::now();

        while state.in_flight > 0 {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let remaining = timeout.saturating_sub(elapsed);
            let result = self.inner.quiescent.wait_timeout(state, remaining);
            state = result.unwrap_or_else(PoisonError::into_inner).0;
        }

        true
    }
}

impl<C: Context> fmt::Debug for DrainPlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrainPlugin")
            .field("draining", &self.is_draining())
            .field("in_flight", &self.in_flight_count())
            .field("rejected_events", &self.rejected_events())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for DrainPlugin<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Context> Clone for DrainPlugin<C> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), _marker: std::marker::PhantomData }
    }
}

impl<C: Context> Observer<C> for DrainPlugin<C> {
    fn on_event_dropped(&self, _machine: &Machine, _event: &Event, reason: &'static str) {
        if reason == "draining" {
            self.inner.rejected_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_stop(&self, _machine: &Machine, _state: &ActiveState<C>) {
        self.initiate_drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_lifecycle_permits_and_quiescence() {
        let drain = DrainPlugin::<serde_json::Value>::new();

        let permit1 = drain.acquire().expect("permit 1");
        let permit2 = drain.acquire().expect("permit 2");
        assert_eq!(drain.in_flight_count(), 2);

        // Initiate drain: permits still held, but new ones rejected
        drain.initiate_drain();
        assert!(drain.is_draining());
        assert_eq!(drain.acquire().err(), Some(DrainingError));
        assert_eq!(drain.rejected_events(), 1);

        // Drop first permit
        drop(permit1);
        assert_eq!(drain.in_flight_count(), 1);

        // Drop second permit -> reaches quiescence
        drop(permit2);
        assert_eq!(drain.in_flight_count(), 0);

        // Wait drained returns immediately true
        assert!(drain.wait_drained(Duration::from_millis(100)));
    }
}
