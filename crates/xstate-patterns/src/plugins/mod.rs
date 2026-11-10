//! Production plugins solving real-world challenges in distributed, concurrent, and resilient Rust services.
//!
//! | Plugin | Real-World Challenge Solved | What It Gives You |
//! |:---|:---|:---|
//! | [`IdempotencyPlugin`] | Webhook replays & concurrent duplicates | LRU deduplication cache, in-flight race prevention, and cached receipts |
//! | [`DrainPlugin`] | The infamous Tokio graceful shutdown problem | In-flight request counting, drain barrier (`wait_drained`), and clean actor teardown |
//! | [`WatchdogPlugin`] | Hung async tasks, deadlocks & SLA breaches | Per-state SLA duration monitoring and automated fallback recovery events |
//! | [`SequenceFencePlugin`] | Out-of-order Kafka/RabbitMQ/Redis messaging | Strict monotonic sequence ordering and gap detection |
//! | [`RateLimiterPlugin`] | Downstream API cascades & traffic spikes | High-throughput token-bucket rate limiting with zero heap allocations |
//! | [`DiagnosticPlugin`] | 3 AM production debugging nightmares | Ring-buffer flight recorder capturing the last N transitions for instant post-mortems |

pub mod diagnostic;
pub mod drain;
pub mod idempotency;
pub mod rate_limit;
pub mod sequence;
pub mod watchdog;

pub use diagnostic::{DiagnosticPlugin, FlightStep};
pub use drain::{DrainPermit, DrainPlugin, DrainingError};
pub use idempotency::{IdempotencyGuard, IdempotencyPlugin, IdempotencyRecord, IdempotencyStatus};
pub use rate_limit::RateLimiterPlugin;
pub use sequence::{FenceOutcome, SequenceFencePlugin};
pub use watchdog::{WATCHDOG_TIMEOUT_EVENT, WatchdogAlert, WatchdogPlugin};
