//! [`MemoryOutboxStore`]: in-memory outbox for tests.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{OutboxRecord, OutboxStore};
use crate::envelope::Envelope;
use crate::error::Result;
use crate::py::{WallClock, system_clock};

#[derive(Default)]
struct Rows {
    entries: Vec<(i64, String, Envelope, bool)>,
    claims: HashMap<i64, (String, f64)>,
    seq: i64,
}

/// In-memory [`OutboxStore`] with leases.
pub struct MemoryOutboxStore {
    rows: Mutex<Rows>,
    clock: WallClock,
}

impl std::fmt::Debug for MemoryOutboxStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryOutboxStore").field("pending", &self.len()).finish_non_exhaustive()
    }
}

impl Default for MemoryOutboxStore {
    fn default() -> Self {
        Self { rows: Mutex::new(Rows::default()), clock: system_clock() }
    }
}

impl MemoryOutboxStore {
    /// An empty store on the system clock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `clock` (epoch seconds) for leases, e.g. a fake clock in tests.
    #[must_use]
    pub fn with_clock(mut self, clock: WallClock) -> Self {
        self.clock = clock;
        self
    }

    fn rows(&self) -> MutexGuard<'_, Rows> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of unsent rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows().entries.iter().filter(|r| !r.3).count()
    }

    /// `true` when nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl OutboxStore for MemoryOutboxStore {
    fn add(&self, topic: &str, envelope: &Envelope) -> Result<()> {
        let mut st = self.rows();
        st.seq += 1;
        let seq = st.seq;
        st.entries.push((seq, topic.to_owned(), envelope.clone(), false));
        Ok(())
    }

    fn pending(&self, limit: usize) -> Result<Vec<OutboxRecord>> {
        Ok(self
            .rows()
            .entries
            .iter()
            .filter(|r| !r.3)
            .take(limit)
            .map(|(s, t, e, _)| OutboxRecord { seq: *s, topic: t.clone(), envelope: e.clone() })
            .collect())
    }

    fn mark_sent(&self, seqs: &[i64]) -> Result<usize> {
        let wanted: HashSet<i64> = seqs.iter().copied().collect();
        let mut n = 0;
        for row in &mut self.rows().entries {
            if wanted.contains(&row.0) && !row.3 {
                row.3 = true;
                n += 1;
            }
        }
        Ok(n)
    }

    fn claim(&self, limit: usize, owner: &str, lease_s: f64) -> Result<Option<Vec<OutboxRecord>>> {
        let now = (self.clock)();
        let mut guard = self.rows();
        let Rows { entries, claims, .. } = &mut *guard;
        let mut out = Vec::new();
        for (s, t, e, sent) in entries.iter() {
            if *sent || out.len() >= limit {
                continue;
            }
            if let Some((holder, until)) = claims.get(s)
                && *until > now
                && holder != owner
            {
                continue;
            }
            claims.insert(*s, (owner.to_owned(), now + lease_s));
            out.push(OutboxRecord { seq: *s, topic: t.clone(), envelope: e.clone() });
        }
        Ok(Some(out))
    }

    fn release(&self, seqs: &[i64], owner: &str) -> Result<Option<usize>> {
        let mut st = self.rows();
        let mut n = 0;
        for s in seqs {
            if st.claims.get(s).is_some_and(|(holder, _)| holder == owner) {
                st.claims.remove(s);
                n += 1;
            }
        }
        Ok(Some(n))
    }
}
