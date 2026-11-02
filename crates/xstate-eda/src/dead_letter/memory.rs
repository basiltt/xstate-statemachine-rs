//! [`MemoryDeadLetterStore`]: the thread-safe in-memory store (Python `DeadLetterStore` /
//! `MemoryDeadLetterStore`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{DeadLetter, DeadLetterSink, DeadLetterStore, check_cutoff};
use crate::error::Result;

#[derive(Default)]
struct Records {
    /// id → (insertion sequence, record); a re-put moves the record to the end.
    rows: HashMap<String, (u64, DeadLetter)>,
    /// insertion sequence → id, oldest first (the eviction order).
    order: BTreeMap<u64, String>,
    seq: u64,
    /// Records dropped to honour the capacity, over the store's life.
    evicted: u64,
    /// A warning was logged for the current run of evictions (re-armed once the store has room
    /// again).
    warned: bool,
}

impl Records {
    fn insert(&mut self, record: DeadLetter) {
        self.seq += 1;
        let id = record.id.clone();
        if let Some((old, _)) = self.rows.insert(id.clone(), (self.seq, record)) {
            self.order.remove(&old);
        }
        self.order.insert(self.seq, id);
    }

    fn remove(&mut self, id: &str) -> bool {
        match self.rows.remove(id) {
            Some((seq, _)) => {
                self.order.remove(&seq);
                true
            }
            None => false,
        }
    }

    /// Drop the oldest records until at most `capacity` remain; one warning per burst.
    fn evict_over(&mut self, capacity: usize) {
        let mut dropped = 0_u64;
        while self.rows.len() > capacity {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.rows.remove(&oldest);
            dropped += 1;
        }
        self.evicted = self.evicted.saturating_add(dropped);
        if dropped == 0 {
            // room again: the next burst of evictions warns afresh
            self.warned = false;
        } else if !self.warned {
            self.warned = true;
            tracing::warn!(
                target: "xstate::eda",
                capacity,
                "the in-memory dead-letter store is full: evicting the oldest records \
                 (use a durable sink to keep them)"
            );
        }
    }
}

/// In-memory dead-letter store; also usable as a sink.
///
/// [`new`](Self::new) is unbounded (every dead letter is kept); a long-lived process should use
/// [`with_capacity`](Self::with_capacity) or a durable store instead.
#[derive(Default)]
pub struct MemoryDeadLetterStore {
    records: Mutex<Records>,
    capacity: Option<usize>,
}

impl std::fmt::Debug for MemoryDeadLetterStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDeadLetterStore")
            .field("len", &self.len())
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl MemoryDeadLetterStore {
    /// An empty, unbounded store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store that keeps at most `capacity` records (at least 1): a put over the limit
    /// evicts the oldest record (counted in [`evicted`](Self::evicted), and logged once per
    /// burst of evictions).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { records: Mutex::new(Records::default()), capacity: Some(capacity.max(1)) }
    }

    fn records(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every record in insertion order (a re-put counts as the newest).
    #[must_use]
    pub fn all(&self) -> Vec<DeadLetter> {
        let recs = self.records();
        recs.order.values().filter_map(|id| recs.rows.get(id)).map(|(_, r)| r.clone()).collect()
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

    /// Records dropped to honour the capacity so far (always 0 for an unbounded store).
    #[must_use]
    pub fn evicted(&self) -> u64 {
        self.records().evicted
    }

    /// Drop every record.
    pub fn clear(&self) {
        let mut recs = self.records();
        recs.rows.clear();
        recs.order.clear();
        recs.warned = false;
    }
}

impl DeadLetterSink for MemoryDeadLetterStore {
    fn put(&self, record: DeadLetter) -> Result<()> {
        let mut recs = self.records();
        recs.insert(record);
        if let Some(capacity) = self.capacity {
            recs.evict_over(capacity);
        }
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
        Ok(self.records().remove(record_id))
    }

    fn purge_older_than(&self, cutoff_wall: f64) -> Result<usize> {
        check_cutoff(cutoff_wall)?;
        let mut recs = self.records();
        let stale: Vec<String> = recs
            .rows
            .iter()
            .filter(|(_, (_, r))| r.taken_at < cutoff_wall)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            recs.remove(id);
        }
        Ok(stale.len())
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
        assert_eq!(store.evicted(), 0);
    }

    #[test]
    fn a_bounded_store_evicts_the_oldest_and_counts_them() {
        let store = MemoryDeadLetterStore::with_capacity(3);
        for (i, id) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
            store.put(rec(id, f64::from(u32::try_from(i).unwrap()))).unwrap();
        }
        assert_eq!(store.len(), 3);
        assert_eq!(store.evicted(), 2);
        let ids: Vec<String> = store.all().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["c", "d", "e"]);
        assert!(store.get("a").unwrap().is_none());
    }

    #[test]
    fn replacing_a_record_in_a_full_store_evicts_nothing() {
        let store = MemoryDeadLetterStore::with_capacity(2);
        store.put(rec("a", 1.0)).unwrap();
        store.put(rec("b", 2.0)).unwrap();
        store.put(rec("a", 3.0)).unwrap();
        assert_eq!((store.len(), store.evicted()), (2, 0));
        let ids: Vec<String> = store.all().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["b", "a"], "the re-put is the newest");
        store.put(rec("c", 4.0)).unwrap();
        assert!(store.get("b").unwrap().is_none(), "the oldest went, not the re-put");
    }

    #[test]
    fn the_eviction_warning_is_rearmed_once_there_is_room_again() {
        let store = MemoryDeadLetterStore::with_capacity(1);
        store.put(rec("a", 1.0)).unwrap();
        store.put(rec("b", 2.0)).unwrap();
        store.put(rec("c", 3.0)).unwrap();
        assert!(store.records().warned, "one warning for the whole burst");
        assert!(store.delete("c").unwrap());
        store.put(rec("d", 4.0)).unwrap();
        assert!(!store.records().warned, "room again: the next burst warns afresh");
        assert_eq!(store.evicted(), 2);
    }

    #[test]
    fn a_zero_capacity_still_keeps_one_record() {
        let store = MemoryDeadLetterStore::with_capacity(0);
        store.put(rec("a", 1.0)).unwrap();
        store.put(rec("b", 2.0)).unwrap();
        assert_eq!(store.len(), 1);
    }
}
