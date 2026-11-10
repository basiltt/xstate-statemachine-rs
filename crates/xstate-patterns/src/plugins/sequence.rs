//! Sequence fence plugin: enforces strict monotonic event ordering and prevents out-of-order execution.
//!
//! Real-world challenge in Rust:
//! In event-driven streaming architectures (Kafka partitions, RabbitMQ retries, Redis Streams, NATS),
//! network retries, parallel consumers, and partition rebalancing frequently cause messages to arrive
//! *out of sequence* (e.g. sequence 5 arrives before sequence 4, or an old replayed sequence 2
//! arrives after sequence 5).
//!
//! If an out-of-order message is processed by a statechart, it can violate business invariants or
//! trigger duplicate, inconsistent actions.
//!
//! [`SequenceFencePlugin`] enforces strict monotonic ordering (`seq > current_seq`) using atomic
//! compare-and-swap operations on the fast path, cleanly dropping stale replays and detecting
//! packet gaps before state changes occur.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use xstate_core::{Context, Event, Machine, Observer};

/// Outcome of evaluating an incoming event sequence number against the fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceOutcome {
    /// Strictly monotonic sequence: permitted to execute.
    Accepted {
        /// The accepted sequence number.
        seq: u64,
    },
    /// Stale or duplicate sequence: older than or equal to the high watermark.
    Stale {
        /// The sequence number received.
        received: u64,
        /// The current high watermark sequence number.
        current: u64,
    },
    /// Sequence jumped ahead, detecting dropped or missing events.
    Gap {
        /// The sequence number received.
        received: u64,
        /// The expected next sequence number.
        expected: u64,
    },
}

/// Enforces strict sequence ordering in event-driven streaming architectures.
pub struct SequenceFencePlugin<C: Context = serde_json::Value> {
    high_watermark: AtomicU64,
    allow_gaps: bool,
    stale_dropped: AtomicU64,
    gaps_detected: AtomicU64,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> SequenceFencePlugin<C> {
    /// Create a new sequence fence initialized to the starting sequence ID (e.g. 0).
    #[must_use]
    pub fn new(initial_seq: u64) -> Self {
        Self {
            high_watermark: AtomicU64::new(initial_seq),
            allow_gaps: true,
            stale_dropped: AtomicU64::new(0),
            gaps_detected: AtomicU64::new(0),
            _marker: std::marker::PhantomData,
        }
    }

    /// Configure whether sequence gaps are accepted (`true`, default) or treated as errors (`false`).
    #[must_use]
    pub fn with_allow_gaps(mut self, allow: bool) -> Self {
        self.allow_gaps = allow;
        self
    }

    /// Evaluate an incoming sequence number against the fence.
    ///
    /// If accepted, atomically advances the high watermark.
    pub fn evaluate(&self, seq: u64) -> FenceOutcome {
        let mut current = self.high_watermark.load(Ordering::Acquire);
        loop {
            if seq <= current {
                self.stale_dropped.fetch_add(1, Ordering::Relaxed);
                return FenceOutcome::Stale { received: seq, current };
            }

            if seq > current + 1 {
                self.gaps_detected.fetch_add(1, Ordering::Relaxed);
                if !self.allow_gaps {
                    return FenceOutcome::Gap { received: seq, expected: current + 1 };
                }
            }

            match self.high_watermark.compare_exchange_weak(
                current,
                seq,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return FenceOutcome::Accepted { seq },
                Err(actual) => current = actual,
            }
        }
    }

    /// Current high watermark sequence number.
    #[must_use]
    pub fn current_sequence(&self) -> u64 {
        self.high_watermark.load(Ordering::Acquire)
    }

    /// Total stale/duplicate events dropped.
    #[must_use]
    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped.load(Ordering::Relaxed)
    }

    /// Total sequence gaps detected.
    #[must_use]
    pub fn gaps_detected(&self) -> u64 {
        self.gaps_detected.load(Ordering::Relaxed)
    }
}

impl<C: Context> fmt::Debug for SequenceFencePlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SequenceFencePlugin")
            .field("current_sequence", &self.current_sequence())
            .field("allow_gaps", &self.allow_gaps)
            .field("stale_dropped", &self.stale_dropped())
            .field("gaps_detected", &self.gaps_detected())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for SequenceFencePlugin<C> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<C: Context> Observer<C> for SequenceFencePlugin<C> {
    fn on_event_dropped(&self, _machine: &Machine, _event: &Event, reason: &'static str) {
        if reason.starts_with("stale_sequence") {
            self.stale_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictly_monotonic_sequences_accepted() {
        let fence = SequenceFencePlugin::<serde_json::Value>::new(10);

        assert_eq!(fence.evaluate(11), FenceOutcome::Accepted { seq: 11 });
        assert_eq!(fence.evaluate(12), FenceOutcome::Accepted { seq: 12 });
        assert_eq!(fence.current_sequence(), 12);

        // Stale duplicate
        assert_eq!(fence.evaluate(11), FenceOutcome::Stale { received: 11, current: 12 });
        assert_eq!(fence.stale_dropped(), 1);
    }

    #[test]
    fn strict_gap_detection() {
        let fence = SequenceFencePlugin::<serde_json::Value>::new(10).with_allow_gaps(false);

        // Jump from 10 to 15 without 11-14
        assert_eq!(fence.evaluate(15), FenceOutcome::Gap { received: 15, expected: 11 });
        assert_eq!(fence.gaps_detected(), 1);
        // Watermark unchanged on gap rejection
        assert_eq!(fence.current_sequence(), 10);
    }
}
