//! [`OutboxRelay`]: move pending outbox rows to a broker, at least once.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{DEFAULT_CLAIM_LEASE_S, OutboxRecord, OutboxStore};
use crate::broker::{BrokerAdapter, SyncBrokerAdapter};
use crate::error::{EdaError, Result};

static NEXT_RELAY: AtomicU64 = AtomicU64::new(1);

/// Move pending [`OutboxStore`] rows to a broker (at least once).
///
/// [`relay_once`](Self::relay_once) publishes up to `batch` rows in `seq` order and marks them
/// sent only after the broker accepted them; a failure leaves the row pending for the next run
/// (rows the broker refused are released at once, and the broker's error is returned).
/// Consumers dedup on the envelope id.
///
/// Several relays may drain one outbox: when the store supports `claim`, each run leases its
/// rows for `lease_s` seconds, so two relays never publish the same row at the same time; a
/// relay that dies mid-batch loses its lease and the rows are picked up again. The duplicate
/// window is one batch per expired lease: size `lease_s` above the slowest batch you expect.
/// An async run that is cancelled (its future dropped) mid-batch cannot release its rows; they
/// wait out the lease, like a relay that died.
pub struct OutboxRelay<B> {
    store: Arc<dyn OutboxStore>,
    broker: B,
    batch: usize,
    owner: String,
    lease_s: f64,
}

impl<B> std::fmt::Debug for OutboxRelay<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxRelay")
            .field("batch", &self.batch)
            .field("owner", &self.owner)
            .field("lease_s", &self.lease_s)
            .finish_non_exhaustive()
    }
}

/// `host:pid:<relay id>`, Python's default owner.
fn default_owner() -> String {
    let host = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "localhost".to_owned());
    format!("{host}:{}:{:x}", std::process::id(), NEXT_RELAY.fetch_add(1, Ordering::Relaxed))
}

impl<B> OutboxRelay<B> {
    /// A relay with batches of 100, the default lease and a `host:pid:id` owner.
    pub fn new(store: Arc<dyn OutboxStore>, broker: B) -> Self {
        Self { store, broker, batch: 100, owner: default_owner(), lease_s: DEFAULT_CLAIM_LEASE_S }
    }

    /// Rows per run.
    #[must_use]
    pub fn batch(mut self, batch: usize) -> Self {
        self.batch = batch;
        self
    }

    /// Lease owner name (unique per relay).
    #[must_use]
    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    /// Lease length in seconds.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] unless `lease_s > 0`.
    pub fn lease_s(mut self, lease_s: f64) -> Result<Self> {
        if lease_s.is_nan() || lease_s <= 0.0 {
            return Err(EdaError::InvalidArgument("lease_s must be > 0".into()));
        }
        self.lease_s = lease_s;
        Ok(self)
    }

    /// The lease owner.
    #[must_use]
    pub fn owner_name(&self) -> &str {
        &self.owner
    }

    /// The broker.
    pub fn broker(&self) -> &B {
        &self.broker
    }

    fn take(&self) -> Result<Vec<OutboxRecord>> {
        match self.store.claim(self.batch, &self.owner, self.lease_s)? {
            Some(rows) => Ok(rows),
            None => self.store.pending(self.batch),
        }
    }

    fn release(&self, left: &[i64]) -> Result<()> {
        if !left.is_empty() {
            self.store.release(left, &self.owner)?;
        }
        Ok(())
    }

    /// Mark what the broker accepted; hand the rest back at once. While the broker's own error
    /// is propagating (`failing`) a failing mark / release is logged, not returned over it.
    fn settle(&self, taken: &[OutboxRecord], sent: &[i64], failing: bool) -> Result<()> {
        let left: Vec<i64> = taken.iter().map(|r| r.seq).filter(|s| !sent.contains(s)).collect();
        if let Err(e) = self.store.mark_sent(sent) {
            if !failing {
                let _ = self.release(&left);
                return Err(e);
            }
            tracing::error!(target: "xstate::eda", error = %e, "outbox relay: mark_sent failed");
        }
        if failing {
            if let Err(e) = self.release(&left) {
                tracing::error!(target: "xstate::eda", error = %e, "outbox relay: release failed");
            }
            Ok(())
        } else {
            self.release(&left)
        }
    }

    fn finish(&self, taken: &[OutboxRecord], sent: &[i64], outcome: Result<()>) -> Result<usize> {
        match outcome {
            Ok(()) => {
                self.settle(taken, sent, false)?;
                Ok(sent.len())
            }
            Err(e) => {
                self.settle(taken, sent, true)?;
                Err(e)
            }
        }
    }
}

impl<B: BrokerAdapter> OutboxRelay<B> {
    /// Claim (or read) up to `batch` pending rows and publish them; returns how many the
    /// broker accepted.
    ///
    /// # Errors
    /// The store's or the broker's error (accepted rows are still marked sent).
    pub async fn relay_once(&self) -> Result<usize> {
        let taken = self.take()?;
        let mut sent = Vec::with_capacity(taken.len());
        let mut outcome = Ok(());
        for rec in &taken {
            if let Err(e) = self.broker.publish(&rec.topic, &rec.envelope).await {
                outcome = Err(e.into());
                break;
            }
            sent.push(rec.seq);
        }
        self.finish(&taken, &sent, outcome)
    }
}

impl<B: SyncBrokerAdapter> OutboxRelay<B> {
    /// [`relay_once`](Self::relay_once) for a [`SyncBrokerAdapter`].
    ///
    /// # Errors
    /// The store's or the broker's error (accepted rows are still marked sent).
    pub fn relay_once_sync(&self) -> Result<usize> {
        let taken = self.take()?;
        let mut sent = Vec::with_capacity(taken.len());
        let mut outcome = Ok(());
        for rec in &taken {
            if let Err(e) = self.broker.publish(&rec.topic, &rec.envelope) {
                outcome = Err(e.into());
                break;
            }
            sent.push(rec.seq);
        }
        self.finish(&taken, &sent, outcome)
    }
}
