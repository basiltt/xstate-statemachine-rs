//! Transactional outbox (Python `eda/outbox.py`): publish what the **chart** says to publish.
//!
//! Selection is declared in the chart, never in code:
//!
//! * a transition with `meta.publish`: `true`, a type string, or
//!   `{"type": "order.paid", "data": ["total", ...]}` (context fields copied into `data`);
//! * a state tagged [`PUBLISH_TAG`]: entering it publishes `xsm.<machine>.transition.<state
//!   key>` with the whole (redacted) context as `data`.
//!
//! [`OutboxPlugin`] collects the envelopes as an [`Observer`](xstate_core::Observer);
//! [`OutboxStore`]s ([`MemoryOutboxStore`], [`SqliteOutboxStore`]) hold them until an
//! [`OutboxRelay`] moves them to a broker (at least once; consumers dedup on the envelope id).

use crate::envelope::Envelope;
use crate::error::Result;

mod memory;
mod plugin;
mod relay;
mod specs;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use memory::MemoryOutboxStore;
pub use plugin::{DEFAULT_MAX_BUFFERED, OutboxFailure, OutboxPlugin};
pub use relay::OutboxRelay;
pub use specs::{PublishSource, PublishSpec, publish_specs};
#[cfg(feature = "sqlite")]
pub use sqlite::{POISON_OWNER, SqliteOutboxStore};

/// The state tag that marks "entering this state is an integration event".
pub const PUBLISH_TAG: &str = "publish";

/// How long a relay owns the rows it claimed before another relay may take them over (a relay
/// that died mid-batch), in seconds.
pub const DEFAULT_CLAIM_LEASE_S: f64 = 30.0;

/// One pending outbox row.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRecord {
    /// Row sequence (publication order).
    pub seq: i64,
    /// Topic to publish on.
    pub topic: String,
    /// The envelope.
    pub envelope: Envelope,
}

/// Where outbox rows live until a relay publishes them.
///
/// `claim` / `release` are optional: a store without them returns `Ok(None)` and the relay
/// falls back to `pending` (single-relay deployments).
pub trait OutboxStore: Send + Sync {
    /// Append a row.
    ///
    /// # Errors
    /// Backend failure or an envelope that cannot be encoded.
    fn add(&self, topic: &str, envelope: &Envelope) -> Result<()>;

    /// Unsent rows in `seq` order, at most `limit`.
    ///
    /// # Errors
    /// Backend failure.
    fn pending(&self, limit: usize) -> Result<Vec<OutboxRecord>>;

    /// Mark rows sent; returns how many were still unsent.
    ///
    /// # Errors
    /// Backend failure.
    fn mark_sent(&self, seqs: &[i64]) -> Result<usize>;

    /// Lease up to `limit` pending rows nobody else holds a live lease on to `owner` for
    /// `lease_s` seconds, in `seq` order. `Ok(None)` when unsupported.
    ///
    /// # Errors
    /// Backend failure.
    fn claim(&self, limit: usize, owner: &str, lease_s: f64) -> Result<Option<Vec<OutboxRecord>>> {
        let _ = (limit, owner, lease_s);
        Ok(None)
    }

    /// Give `owner`'s leases on `seqs` back; returns how many. `Ok(None)` when unsupported.
    ///
    /// # Errors
    /// Backend failure.
    fn release(&self, seqs: &[i64], owner: &str) -> Result<Option<usize>> {
        let _ = (seqs, owner);
        Ok(None)
    }
}

impl<T: OutboxStore + ?Sized> OutboxStore for std::sync::Arc<T> {
    fn add(&self, topic: &str, envelope: &Envelope) -> Result<()> {
        (**self).add(topic, envelope)
    }
    fn pending(&self, limit: usize) -> Result<Vec<OutboxRecord>> {
        (**self).pending(limit)
    }
    fn mark_sent(&self, seqs: &[i64]) -> Result<usize> {
        (**self).mark_sent(seqs)
    }
    fn claim(&self, limit: usize, owner: &str, lease_s: f64) -> Result<Option<Vec<OutboxRecord>>> {
        (**self).claim(limit, owner, lease_s)
    }
    fn release(&self, seqs: &[i64], owner: &str) -> Result<Option<usize>> {
        (**self).release(seqs, owner)
    }
}
