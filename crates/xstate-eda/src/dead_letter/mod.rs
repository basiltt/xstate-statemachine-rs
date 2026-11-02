//! Dead letters: the record, the store contract and its stores, the broker sink, the chart
//! plugin and replay (Python `patterns/dead_letter.py` + `eda/dead_letter.py`).
//!
//! [`DeadLetterPlugin`] captures chart-driven dead letters; [`InboundDispatcher`] adds
//! envelope-driven ones (poison after `max_attempts`, `unknown_event`, corrupt envelopes). Both
//! produce the same [`DeadLetter`] record. Every record is redacted before it is written (the
//! plugin and dispatcher do it; the SQLite store's `put` does it again for records built by
//! hand), and every replay / purge can be audited.
//!
//! [`InboundDispatcher`]: crate::InboundDispatcher

use xstate_core::redact::RedactKeys;

use crate::error::Result;

mod memory;
mod plugin;
mod record;
mod replay;
mod sink;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use memory::MemoryDeadLetterStore;
pub use plugin::DeadLetterPlugin;
pub use record::{DeadLetter, ErrorEntry};
pub use replay::{ReplayOptions, ReplayResult, replay_dead_letter};
pub use sink::{BrokerDeadLetterSink, FnSink, sink_fn};
#[cfg(feature = "sqlite")]
pub use sqlite::{AuditEntry, SqliteDeadLetterStore};

/// The state tag that marks a dead-letter terminal state in the chart.
pub const DEAD_LETTER_TAG: &str = "dead-letter";
/// Context key a chart may keep its error chain under; it is merged into captured records.
pub const ERRORS_CONTEXT_KEY: &str = "_xsm_errors";
/// Default bound on the error chain.
pub const DEFAULT_MAX_ERRORS: usize = 20;
/// Error messages are truncated to this many characters.
pub const MAX_ERROR_MESSAGE: usize = 1000;

/// `orders` → `orders.dlq`.
#[must_use]
pub fn dlq_topic(topic: &str) -> String {
    format!("{topic}.dlq")
}

/// `record` with its event payload, snapshot and envelope redacted.
#[must_use]
pub fn redact_record(record: &DeadLetter, keys: &RedactKeys) -> DeadLetter {
    let mut out = record.clone();
    keys.redact_in_place(&mut out.event);
    keys.redact_in_place(&mut out.snapshot);
    if let Some(env) = out.envelope.as_mut() {
        keys.redact_in_place(env);
    }
    out
}

/// Anything a dead letter can be handed to (Python: any `callable(DeadLetter)`).
pub trait DeadLetterSink: Send + Sync {
    /// Store or forward one record.
    ///
    /// # Errors
    /// Backend or broker failure; the record was not stored.
    fn put(&self, record: DeadLetter) -> Result<()>;
}

/// What a dead-letter store offers the plugin, the dispatcher and an operator CLI.
pub trait DeadLetterStore: DeadLetterSink {
    /// The record with `record_id`.
    ///
    /// # Errors
    /// Backend failure.
    fn get(&self, record_id: &str) -> Result<Option<DeadLetter>>;

    /// Records oldest first (by `(taken_at, id)`), unresolved unless `include_resolved`, at
    /// most `limit`.
    ///
    /// # Errors
    /// Backend failure.
    fn list(&self, include_resolved: bool, limit: usize) -> Result<Vec<DeadLetter>>;

    /// Set `resolved_at`; `true` when the record exists.
    ///
    /// # Errors
    /// Backend failure.
    fn mark_resolved(&self, record_id: &str, when: f64) -> Result<bool>;

    /// Delete a record; `true` when one went.
    ///
    /// # Errors
    /// Backend failure.
    fn delete(&self, record_id: &str) -> Result<bool>;

    /// Drop records with `taken_at < cutoff_wall`; returns how many (retention: dead letters
    /// hold snapshots and must not accumulate forever).
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`](crate::EdaError::InvalidArgument) for a NaN cutoff (it
    /// used to delete everything); backend failure.
    fn purge_older_than(&self, cutoff_wall: f64) -> Result<usize>;

    /// Number of records.
    ///
    /// # Errors
    /// Backend failure.
    fn count(&self) -> Result<usize>;

    /// Append an operator action (replay / purge) to the audit trail. Stores without one
    /// ignore it (the default).
    ///
    /// # Errors
    /// Backend failure.
    fn audit(
        &self,
        action: &str,
        record_id: Option<&str>,
        reason: &str,
        detail: &serde_json::Value,
        actor: Option<&str>,
    ) -> Result<()> {
        let _ = (action, record_id, reason, detail, actor);
        Ok(())
    }
}

pub(crate) fn check_cutoff(cutoff_wall: f64) -> Result<()> {
    if cutoff_wall.is_nan() {
        return Err(crate::EdaError::InvalidArgument("cutoff_wall must not be NaN".into()));
    }
    Ok(())
}
