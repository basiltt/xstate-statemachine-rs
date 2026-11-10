//! Rate limiter plugin: protects machines and downstream dependencies against burst traffic.
//!
//! Real-world challenge in Rust:
//! Microservices calling third-party APIs (Stripe, `OpenAI`, external payment gateways) easily exceed
//! rate limits or suffer cascading degradation under traffic spikes.
//!
//! Traditional HTTP middleware rate-limits at the network boundary, but cannot enforce state-dependent
//! limits (e.g. allowing `CANCEL` or `STATUS_CHECK` freely while strictly throttling `EXECUTE_TRADE`
//! or `CHECKOUT`).
//!
//! [`RateLimiterPlugin`] implements a high-throughput token-bucket rate limiter that enforces
//! burst capacity and refill rates directly before statechart microsteps execute.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use xstate_core::{Context, Event, Machine, Observer};

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// Token-bucket rate limiter plugin.
pub struct RateLimiterPlugin<C: Context = serde_json::Value> {
    capacity: f64,
    refill_rate_per_sec: f64,
    state: Mutex<BucketState>,
    throttled_events: AtomicU64,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> RateLimiterPlugin<C> {
    /// Create a new token-bucket rate limiter.
    ///
    /// - `capacity`: maximum burst capacity in tokens.
    /// - `refill_rate_per_sec`: tokens added per second.
    #[must_use]
    pub fn new(capacity: f64, refill_rate_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_rate_per_sec,
            state: Mutex::new(BucketState { tokens: capacity, last_refill: Instant::now() }),
            throttled_events: AtomicU64::new(0),
            _marker: std::marker::PhantomData,
        }
    }

    /// Attempt to acquire `cost` tokens (typically 1.0 per event).
    ///
    /// Returns `true` if permitted, or `false` if rate-limited.
    pub fn try_acquire(&self, cost: f64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.last_refill).as_secs_f64();

        // Refill tokens
        state.tokens = (state.tokens + elapsed * self.refill_rate_per_sec).min(self.capacity);
        state.last_refill = now;

        if state.tokens >= cost {
            state.tokens -= cost;
            true
        } else {
            self.throttled_events.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Available token balance.
    #[must_use]
    pub fn available_tokens(&self) -> f64 {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.last_refill).as_secs_f64();
        (state.tokens + elapsed * self.refill_rate_per_sec).min(self.capacity)
    }

    /// Total events throttled by this limiter.
    #[must_use]
    pub fn throttled_events(&self) -> u64 {
        self.throttled_events.load(Ordering::Relaxed)
    }
}

impl<C: Context> fmt::Debug for RateLimiterPlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimiterPlugin")
            .field("capacity", &self.capacity)
            .field("refill_rate_per_sec", &self.refill_rate_per_sec)
            .field("throttled_events", &self.throttled_events())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for RateLimiterPlugin<C> {
    fn default() -> Self {
        Self::new(100.0, 10.0)
    }
}

impl<C: Context> Observer<C> for RateLimiterPlugin<C> {
    fn on_event_dropped(&self, _machine: &Machine, _event: &Event, reason: &'static str) {
        if reason.starts_with("rate_limited") {
            self.throttled_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_burst_and_exhaustion() {
        let limiter = RateLimiterPlugin::<serde_json::Value>::new(3.0, 0.1);

        assert!(limiter.try_acquire(1.0));
        assert!(limiter.try_acquire(1.0));
        assert!(limiter.try_acquire(1.0));

        // Exhausted
        assert!(!limiter.try_acquire(1.0));
        assert_eq!(limiter.throttled_events(), 1);
    }
}
