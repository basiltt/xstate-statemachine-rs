//! Resilience patterns for [`xstate_core`] machines.
//!
//! Each pattern is a thin layer over the statechart engine rather than a runtime of its own:
//!
//! | Module | Pattern | What it gives you |
//! |--------|---------|-------------------|
//! | [`retry`] | [`RetryPolicy`] | exponential backoff with jitter as named delay / guard / actions |
//! | [`circuit_breaker`] | [`CircuitBreaker`] | Nygard's breaker as a three-state chart on a `SyncInterpreter` |
//! | [`saga`] | [`SagaBuilder`] | orchestrated sagas emitted as plain XState machine JSON |
//! | [`choreography`] | [`ChoreographyRouter`] | an inbound dispatcher over a `type → machine` map |
//!
//! All four are ports of the Python library's `patterns/` package; the machine JSON they emit
//! is byte-for-byte what the Python builders emit, so charts can be inspected, diffed and
//! edited with the same tooling.
//!
//! ```
//! use xstate_patterns::{JitterMode, RetryPolicy};
//!
//! let policy = RetryPolicy::new(3, 100.0, 2.0, 1_000.0, JitterMode::None)?;
//! assert_eq!(policy.delay_for(1)?, 100.0);
//! assert_eq!(policy.delay_for(2)?, 200.0);
//! assert!(policy.should_retry(2));
//! assert!(!policy.should_retry(3));
//! # Ok::<(), xstate_patterns::RetryError>(())
//! ```

#![forbid(unsafe_code)]

pub mod choreography;
pub mod circuit_breaker;
mod ctx;
pub mod retry;
pub mod saga;

pub use choreography::{
    ChoreographyError, ChoreographyRouter, PersistedHandler, Route, Routes, instance_key,
};
pub use circuit_breaker::{
    AdmitError, CIRCUIT_BREAKER_CONFIG, CallError, CircuitBreaker, CircuitBreakerBuilder,
    CircuitBreakerError, CircuitOpenError, CircuitState, Permit, circuit_breaker_config,
    circuit_breaker_logic, wrap,
};
pub use retry::{JitterMode, RetryError, RetryPolicy};
pub use saga::{SagaBuilder, SagaError, SagaStep};

/// Context key under which the dead-letter plugin keeps its error chain.
///
/// The retry pattern clears it on success so that "attempts" and "error chain" stay consistent
/// (a success means the next failure path starts fresh). Defined here because the retry logic
/// needs it; `xstate-eda` re-exports the same constant.
pub const ERRORS_CONTEXT_KEY: &str = "_xsm_errors";
