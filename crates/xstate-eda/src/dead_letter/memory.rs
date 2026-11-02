//! [`MemoryDeadLetterStore`]: the thread-safe in-memory store (Python `DeadLetterStore` /
//! `MemoryDeadLetterStore`).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{DeadLetter, DeadLetterSink, DeadLetterStore, check_cutoff};
use crate::error::Result;

#[derive(Default)]
struct Records {
    /// id → (insertion sequence, record); a re-put moves the record to the end.
    rows: HashMap<String, (u64, DeadLetter)>,
    seq: u64,
}

/// In-memory dead-letter store; also usable as a sink.
#[derive(Default)]
pub struct MemoryDeadLetterStore {
    records: Mutex<Records>,
}

impl std::fmt::Debug for MemoryDeadLetterStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDeadLetterStore").field("len", &self.len()).finish()
    }
}

impl MemoryDeadLetterStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every record in insertion order (a re-put counts as the newest).
    #[must_use]
    pub fn all(&self) -> Vec<DeadLetter> {
        let recs = self.records();
        let mut rows: Vec<&(u64, DeadLetter)> = recs.rows.values().collect();
        rows.sort_by_key(|(seq, _)| *seq);
        rows.into_iter().map(|(_, r)| r.clone()).collect()
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records().rows.len()
    }

    /// `true` when the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every record.
    pub fn clear(&self) {
        self.records().rows.clear();
    }
}

impl DeadLetterSink for MemoryDeadLetterStore {
    fn put(&self, record: DeadLetter) -> Result<()> {
        let mut recs = self.records();
        recs.seq += 1;
        let seq = recs.seq;
        recs.rows.insert(record.id.clone(), (seq, record));
        Ok(())
    }
}

impl DeadLetterStore for MemoryDeadLetterStore {
    fn get(&self, record_id: &str) -> Result<Option<DeadLetter>> {
        Ok(self.records().rows.get(record_id).map(|(_, r)| r.clone()))
    }

    fn list(&self, include_resolved: bool, limit: usize) -> Result<Vec<DeadLetter>> {
        let mut rows: Vec<DeadLetter> = self
            .records()
            .rows
            .values()
            .filter(|(_, r)| include_resolved || r.resolved_at.is_none())
            .map(|(_, r)| r.clone())
            .collect();
        rows.sort_by(|a, b| a.taken_at.total_cmp(&b.taken_at).then_with(|| a.id.cmp(&b.id)));
        rows.truncate(limit);
        Ok(rows)
    }

    fn mark_resolved(&self, record_id: &str, when: f64) -> Result<bool> {
        Ok(self
            .records()
            .rows
            .get_mut(record_id)
            .map(|(_, r)| r.resolved_at = Some(when))
            .is_some())
    }

    fn delete(&self, record_id: &str) -> Result<bool> {
        Ok(self.records().rows.remove(record_id).is_some())
    }

    fn purge_older_than(&self, cutoff_wall: f64) -> Result<usize> {
        check_cutoff(cutoff_wall)?;
        let mut recs = self.records();
        let before = recs.rows.len();
        recs.rows.retain(|_, (_, r)| r.taken_at >= cutoff_wall);
        Ok(before - recs.rows.len())
    }

    fn count(&self) -> Result<usize> {
        Ok(self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(id: &str, at: f64) -> DeadLetter {
        let mut r = DeadLetter::new("m", "m.dead", json!({"type": "E", "payload": {}}), at);
        r.id = id.into();
        r
    }

    #[test]
    fn put_replaces_and_lists_oldest_first() {
        let store = MemoryDeadLetterStore::new();
        store.put(rec("b", 2.0)).unwrap();
        store.put(rec("a", 2.0)).unwrap();
        store.put(rec("c", 1.0)).unwrap();
        store.put(rec("b", 3.0)).unwrap();
        assert_eq!(store.len(), 3);
        let ids: Vec<String> = store.list(false, 10).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["c", "a", "b"]);
        assert_eq!(store.all().last().unwrap().id, "b");
        assert!(store.mark_resolved("a", 9.0).unwrap());
        assert!(!store.mark_resolved("zz", 9.0).unwrap());
        assert_eq!(store.list(false, 10).unwrap().len(), 2);
        assert_eq!(store.list(true, 1).unwrap().len(), 1);
        assert!(store.purge_older_than(f64::NAN).is_err());
        assert_eq!(store.purge_older_than(2.5).unwrap(), 2);
        assert!(store.delete("b").unwrap());
        assert!(store.is_empty());
    }
}
