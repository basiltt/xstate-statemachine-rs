//! Idempotency plugin: guarantees at-most-once transition execution for webhooks and retried events.
//!
//! Real-world challenge in Rust:
//! Webhooks (e.g. Stripe, GitHub, Shopify) and network retries frequently resend events with identical
//! keys. Without stateful idempotency, concurrent or replayed events execute transitions twice,
//! producing duplicate billing, double shipments, or corrupted balances.
//!
//! Hand-rolling idempotency in application handlers is error-prone, especially during the race
//! window when two concurrent requests arrive before either has finished persisting.
//!
//! [`IdempotencyPlugin`] solves this directly on the statechart with an in-memory or store-backed
//! LRU cache that detects:
//! - **New events**: permitted to execute.
//! - **In-flight concurrent events**: drops or yields early while the first request finishes.
//! - **Completed duplicates**: returns the cached state receipt without re-executing actions.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use smol_str::SmolStr;
use xstate_core::{ActiveState, Context, Event, Machine, Observer, StepReport};

/// Cached record of a completed statechart event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyRecord {
    /// The idempotency key.
    pub key: SmolStr,
    /// Type of the processed event.
    pub event_type: SmolStr,
    /// Active leaf states after processing.
    pub leaf_states: Vec<SmolStr>,
    /// Whether any state transition occurred.
    pub changed: bool,
    /// When processing finished.
    pub processed_at: Instant,
}

#[derive(Debug, Clone)]
enum KeyState {
    InFlight { _started_at: Instant },
    Completed(IdempotencyRecord),
}

/// Status of an idempotency check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyStatus {
    /// The key is new; the event may proceed.
    New,
    /// A transition with this key is currently in-flight on another thread.
    InFlight,
    /// This key was already processed to completion. Contains cached execution record.
    Duplicate(IdempotencyRecord),
}

/// RAII guard representing an in-flight idempotent operation.
///
/// If dropped without calling [`IdempotencyGuard::commit`], the in-flight reservation
/// is cleared so subsequent retries are not blocked.
pub struct IdempotencyGuard<'a, C: Context> {
    plugin: &'a IdempotencyPlugin<C>,
    key: SmolStr,
    committed: bool,
}

impl<C: Context> IdempotencyGuard<'_, C> {
    /// Commit the completed record, storing it in the cache.
    pub fn commit(
        mut self,
        event_type: impl Into<SmolStr>,
        leaf_states: Vec<SmolStr>,
        changed: bool,
    ) {
        self.committed = true;
        let record = IdempotencyRecord {
            key: self.key.clone(),
            event_type: event_type.into(),
            leaf_states,
            changed,
            processed_at: Instant::now(),
        };
        self.plugin.record_completed(record);
    }
}

/// Thread-safe closure extracting idempotency keys from events.
pub type KeyExtractor = Arc<dyn Fn(&Event) -> Option<SmolStr> + Send + Sync>;

impl<C: Context> fmt::Debug for IdempotencyGuard<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdempotencyGuard")
            .field("key", &self.key)
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

impl<C: Context> Drop for IdempotencyGuard<'_, C> {
    fn drop(&mut self) {
        if !self.committed {
            self.plugin.clear(&self.key);
        }
    }
}

struct InnerStore {
    map: HashMap<SmolStr, KeyState>,
    queue: VecDeque<SmolStr>,
    capacity: usize,
}

impl InnerStore {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity.min(1024)),
            queue: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    fn check(&self, key: &str) -> IdempotencyStatus {
        match self.map.get(key) {
            None => IdempotencyStatus::New,
            Some(KeyState::InFlight { .. }) => IdempotencyStatus::InFlight,
            Some(KeyState::Completed(record)) => IdempotencyStatus::Duplicate(record.clone()),
        }
    }

    fn admit(&mut self, key: SmolStr) -> Result<(), IdempotencyStatus> {
        match self.map.get(key.as_str()) {
            None => {
                if self.queue.len() >= self.capacity
                    && let Some(oldest) = self.queue.pop_front()
                {
                    self.map.remove(&oldest);
                }
                self.queue.push_back(key.clone());
                self.map.insert(key, KeyState::InFlight { _started_at: Instant::now() });
                Ok(())
            }
            Some(KeyState::InFlight { .. }) => Err(IdempotencyStatus::InFlight),
            Some(KeyState::Completed(r)) => Err(IdempotencyStatus::Duplicate(r.clone())),
        }
    }

    fn record_completed(&mut self, record: IdempotencyRecord) {
        let key = record.key.clone();
        if !self.map.contains_key(&key) {
            if self.queue.len() >= self.capacity
                && let Some(oldest) = self.queue.pop_front()
            {
                self.map.remove(&oldest);
            }
            self.queue.push_back(key.clone());
        }
        self.map.insert(key, KeyState::Completed(record));
    }

    fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }
}

/// Bounded, thread-safe idempotency plugin.
pub struct IdempotencyPlugin<C: Context = serde_json::Value> {
    store: Mutex<InnerStore>,
    duplicates_intercepted: AtomicU64,
    key_extractor: Option<KeyExtractor>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> IdempotencyPlugin<C> {
    /// Create a new idempotency plugin with bounded cache capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            store: Mutex::new(InnerStore::new(capacity)),
            duplicates_intercepted: AtomicU64::new(0),
            key_extractor: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Set a key extractor closure to automatically derive idempotency keys from incoming events.
    #[must_use]
    pub fn with_extractor<F>(mut self, extractor: F) -> Self
    where
        F: Fn(&Event) -> Option<SmolStr> + Send + Sync + 'static,
    {
        self.key_extractor = Some(Arc::new(extractor));
        self
    }

    /// Check the idempotency status of a key without reserving it.
    #[must_use]
    pub fn check(&self, key: &str) -> IdempotencyStatus {
        let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.check(key)
    }

    /// Attempt to admit an operation with the given idempotency key.
    ///
    /// # Errors
    /// Returns [`IdempotencyStatus::InFlight`] if another thread is currently processing this key,
    /// or [`IdempotencyStatus::Duplicate`] if this key was already processed.
    pub fn admit(
        &self,
        key: impl Into<SmolStr>,
    ) -> Result<IdempotencyGuard<'_, C>, IdempotencyStatus> {
        let key = key.into();
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        match store.admit(key.clone()) {
            Ok(()) => Ok(IdempotencyGuard { plugin: self, key, committed: false }),
            Err(status) => {
                if matches!(status, IdempotencyStatus::Duplicate(_)) {
                    self.duplicates_intercepted.fetch_add(1, Ordering::Relaxed);
                }
                Err(status)
            }
        }
    }

    /// Directly record a completed transition.
    pub fn record_completed(&self, record: IdempotencyRecord) {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.record_completed(record);
    }

    /// Clear a key from the cache (e.g. after a fatal downstream error).
    pub fn clear(&self, key: &str) {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.remove(key);
    }

    /// Total duplicate events intercepted by this plugin.
    #[must_use]
    pub fn duplicates_intercepted(&self) -> u64 {
        self.duplicates_intercepted.load(Ordering::Relaxed)
    }
}

impl<C: Context> fmt::Debug for IdempotencyPlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdempotencyPlugin")
            .field("duplicates_intercepted", &self.duplicates_intercepted())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for IdempotencyPlugin<C> {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl<C: Context> Observer<C> for IdempotencyPlugin<C> {
    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        if let Some(extractor) = &self.key_extractor
            && let Some(key) = extractor(event)
        {
            let record = IdempotencyRecord {
                key,
                event_type: event.ty.clone(),
                leaf_states: state.leaf_ids(machine).into_iter().map(SmolStr::new).collect(),
                changed: report.changed,
                processed_at: Instant::now(),
            };
            self.record_completed(record);
        }
    }

    fn on_event_dropped(&self, _machine: &Machine, _event: &Event, reason: &'static str) {
        if reason.starts_with("idempotent") {
            self.duplicates_intercepted.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_key_admits_and_commits() {
        let plugin = IdempotencyPlugin::<serde_json::Value>::new(100);
        let guard = plugin.admit("tx-101").expect("admit new key");
        assert_eq!(plugin.check("tx-101"), IdempotencyStatus::InFlight);

        guard.commit("PAY", vec![SmolStr::new("paid")], true);

        match plugin.check("tx-101") {
            IdempotencyStatus::Duplicate(rec) => {
                assert_eq!(rec.event_type.as_str(), "PAY");
                assert_eq!(rec.leaf_states, vec![SmolStr::new("paid")]);
                assert!(rec.changed);
            }
            other => panic!("expected duplicate, got {other:?}"),
        }
    }

    #[test]
    fn uncommitted_guard_releases_in_flight() {
        let plugin = IdempotencyPlugin::<serde_json::Value>::new(100);
        {
            let _guard = plugin.admit("tx-uncommitted").unwrap();
            assert_eq!(plugin.check("tx-uncommitted"), IdempotencyStatus::InFlight);
        }
        // Dropped without commit -> released
        assert_eq!(plugin.check("tx-uncommitted"), IdempotencyStatus::New);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let plugin = IdempotencyPlugin::<serde_json::Value>::new(2);
        plugin.record_completed(IdempotencyRecord {
            key: SmolStr::new("k1"),
            event_type: SmolStr::new("A"),
            leaf_states: vec![],
            changed: true,
            processed_at: Instant::now(),
        });
        plugin.record_completed(IdempotencyRecord {
            key: SmolStr::new("k2"),
            event_type: SmolStr::new("B"),
            leaf_states: vec![],
            changed: true,
            processed_at: Instant::now(),
        });
        plugin.record_completed(IdempotencyRecord {
            key: SmolStr::new("k3"),
            event_type: SmolStr::new("C"),
            leaf_states: vec![],
            changed: true,
            processed_at: Instant::now(),
        });

        assert_eq!(plugin.check("k1"), IdempotencyStatus::New); // evicted
        assert!(matches!(plugin.check("k2"), IdempotencyStatus::Duplicate(_)));
        assert!(matches!(plugin.check("k3"), IdempotencyStatus::Duplicate(_)));
    }
}
