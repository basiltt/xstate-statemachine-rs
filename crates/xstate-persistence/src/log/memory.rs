//! In-memory transition log.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::record::{TransitionLogStore, TransitionRecord, check_append, check_cutoff};
use crate::store::Result;

/// In-memory [`TransitionLogStore`]: per-machine lists behind a lock. Records live as long as
/// the object; for tests and single-process use.
#[derive(Debug, Default)]
pub struct MemoryLog {
    rows: Mutex<HashMap<String, Vec<TransitionRecord>>>,
}

impl MemoryLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of records across all keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows().values().map(Vec::len).sum()
    }

    /// `true` when no record is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn rows(&self) -> MutexGuard<'_, HashMap<String, Vec<TransitionRecord>>> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TransitionLogStore for MemoryLog {
    fn append(&self, record: &TransitionRecord) -> Result<()> {
        check_append(record)?;
        self.rows().entry(record.machine_id.clone()).or_default().push(record.clone());
        Ok(())
    }

    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        let mut rows = self.rows();
        let list = rows.entry(record.machine_id.clone()).or_default();
        let seq = list.last().map_or(1, |r| r.seq + 1);
        let stored = TransitionRecord { seq, ..record };
        list.push(stored.clone());
        Ok(stored)
    }

    fn next_seq(&self, machine_id: &str) -> Result<u64> {
        Ok(self.rows().get(machine_id).and_then(|l| l.last()).map_or(1, |r| r.seq + 1))
    }

    fn read(
        &self,
        machine_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TransitionRecord>> {
        Ok(self
            .rows()
            .get(machine_id)
            .map(|list| list.iter().filter(|r| r.seq > after_seq).take(limit).cloned().collect())
            .unwrap_or_default())
    }

    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64> {
        let cutoff = check_cutoff(cutoff_ts)?;
        let mut removed = 0usize;
        for list in self.rows().values_mut() {
            let before = list.len();
            list.retain(|r| r.ts >= cutoff);
            removed += before - list.len();
        }
        Ok(u64::try_from(removed).unwrap_or(u64::MAX))
    }

    fn forget(&self, machine_id: &str) -> Result<u64> {
        let removed = self.rows().remove(machine_id).map_or(0, |l| l.len());
        Ok(u64::try_from(removed).unwrap_or(u64::MAX))
    }
}
