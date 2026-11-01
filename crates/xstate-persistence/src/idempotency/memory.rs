//! In-memory inbox.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{InboxEntry, InboxStore, count, expiry, is_live};
use crate::store::{Result, wall_now};

/// Map-backed inbox for tests and single-process apps.
#[derive(Debug, Default)]
pub struct MemoryInbox {
    rows: Mutex<HashMap<(String, String), InboxEntry>>,
}

impl MemoryInbox {
    /// An empty inbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries, expired ones included until purged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows().len()
    }

    /// `true` when nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows().is_empty()
    }

    fn rows(&self) -> MutexGuard<'_, HashMap<(String, String), InboxEntry>> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl InboxStore for MemoryInbox {
    fn get(&self, scope: &str, key: &str) -> Result<Option<InboxEntry>> {
        let now = wall_now();
        let rows = self.rows();
        Ok(rows
            .get(&(scope.to_owned(), key.to_owned()))
            .filter(|e| is_live(e.expires_at, now))
            .cloned())
    }

    fn claim(&self, scope: &str, key: &str, fingerprint: &str, ttl_s: Option<f64>) -> Result<bool> {
        let now = wall_now();
        let mut rows = self.rows();
        let id = (scope.to_owned(), key.to_owned());
        if rows.get(&id).is_some_and(|e| is_live(e.expires_at, now)) {
            return Ok(false);
        }
        rows.insert(
            id,
            InboxEntry {
                fingerprint: fingerprint.to_owned(),
                receipt_json: None,
                expires_at: expiry(ttl_s, now),
            },
        );
        Ok(true)
    }

    fn mark(&self, scope: &str, key: &str, receipt_json: &str, ttl_s: Option<f64>) -> Result<()> {
        let now = wall_now();
        let mut rows = self.rows();
        let id = (scope.to_owned(), key.to_owned());
        let fingerprint = rows.get(&id).map(|e| e.fingerprint.clone()).unwrap_or_default();
        rows.insert(
            id,
            InboxEntry {
                fingerprint,
                receipt_json: Some(receipt_json.to_owned()),
                expires_at: expiry(ttl_s, now),
            },
        );
        Ok(())
    }

    fn release(&self, scope: &str, key: &str) -> Result<()> {
        let mut rows = self.rows();
        let id = (scope.to_owned(), key.to_owned());
        if rows.get(&id).is_some_and(|e| e.receipt_json.is_none()) {
            rows.remove(&id);
        }
        Ok(())
    }

    fn purge_expired(&self, now: Option<f64>) -> Result<u64> {
        let now = now.unwrap_or_else(wall_now);
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|_, e| is_live(e.expires_at, now));
        Ok(count(before - rows.len()))
    }

    fn forget(&self, scope: &str) -> Result<u64> {
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|(s, _), _| s != scope);
        Ok(count(before - rows.len()))
    }
}
