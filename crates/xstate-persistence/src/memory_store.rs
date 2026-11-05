//! [`MemoryStore`], the in-process store, and the policy shared by every built-in backend.
//!
//! The Python library factors the policy (key validation, size cap on save *and* load,
//! `machine_version` rules, record-shape checks) into `BaseStore`; here it lives in
//! `StorePolicy` plus a few record-validation helpers, all crate-private and reused by
//! `file_store` and `sqlite_store` so the three backends cannot drift apart.
//!
//! Python reference: `persistence/store.py` (`BaseStore`, `MemoryStore`, `check_record_fields`,
//! `check_save_args`) and `persistence/deadline.py` (`check_deadline_record`).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use serde_json::Value;
use xstate_core::snapshot::Deadline;

use crate::store::Result;
use crate::store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, SaveOptions, StateStore,
    StoreError, StoredSnapshot, check_size, validate_key, validate_machine_version, wall_now,
};

/// Integers in deadline records must stay below `2**63` (Python `check_deadline_record`).
const INT64_LIMIT: u64 = 1 << 63;
/// Waiting longer than this is treated as "forever" (guards `Instant` overflow).
const FOREVER: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// Lock a mutex, recovering the data if a previous holder panicked.
pub(crate) fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The deadline `timeout` from now.
pub(crate) fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).or_else(|| now.checked_add(FOREVER)).unwrap_or(now)
}

// ---------------------------------------------------------------------------------------
// Shared policy
// ---------------------------------------------------------------------------------------

/// The policy every store applies around its backend primitives: validate the key, validate
/// `machine_version` and deadlines, enforce the size cap on save and on load.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StorePolicy {
    max_snapshot_bytes: usize,
}

impl Default for StorePolicy {
    fn default() -> Self {
        Self { max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES }
    }
}

impl StorePolicy {
    /// A policy with the given size cap (at least 1 byte).
    pub(crate) fn with_max_snapshot_bytes(max_snapshot_bytes: usize) -> Self {
        Self { max_snapshot_bytes: max_snapshot_bytes.max(1) }
    }

    /// The configured cap.
    pub(crate) fn max_snapshot_bytes(self) -> usize {
        self.max_snapshot_bytes
    }

    /// Validate a `save` call and return the deadlines normalised for storage (`delay_ms` as a
    /// plain integer).
    pub(crate) fn prepare_save(
        self,
        key: &str,
        snapshot_json: &str,
        options: &SaveOptions<'_>,
    ) -> Result<Vec<Deadline>> {
        validate_key(key)?;
        validate_machine_version(options.machine_version)?;
        let deadlines =
            options.deadlines.iter().map(normalize_deadline).collect::<Result<Vec<_>>>()?;
        check_size(key, snapshot_json, self.max_snapshot_bytes)?;
        Ok(deadlines)
    }

    /// Enforce the cap on a record read back from storage (a record another writer poisoned
    /// must not make this process allocate unboundedly).
    pub(crate) fn check_loaded(self, key: &str, snapshot_json: &str) -> Result<()> {
        check_size(key, snapshot_json, self.max_snapshot_bytes)
    }
}

/// `delay_ms` as a non-negative `i64`, accepting whole floats (`1000.0`).
pub(crate) fn delay_ms_as_i64(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(n) = number.as_i64() {
        return (n >= 0).then_some(n);
    }
    let f = number.as_f64()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let whole = (f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f < INT64_LIMIT as f64)
        .then_some(f as i64);
    whole
}

fn invalid_deadline(why: &str) -> StoreError {
    StoreError::Other(format!("invalid deadline: {why}"))
}

/// Check a deadline on its way into a store and normalise its `delay_ms` to an integer, so the
/// record always loads back in Python (whose `Deadline.delay_ms` is an `int`).
pub(crate) fn normalize_deadline(d: &Deadline) -> Result<Deadline> {
    if d.state_id.is_empty() {
        return Err(invalid_deadline("'state_id' must be a non-empty string"));
    }
    if d.event_type.is_empty() {
        return Err(invalid_deadline("'event_type' must be a non-empty string"));
    }
    if d.entry_seq >= INT64_LIMIT {
        return Err(invalid_deadline("'entry_seq' must be below 2**63"));
    }
    if !d.due_at_wall.is_finite() {
        return Err(invalid_deadline("'due_at_wall' must be a finite number"));
    }
    let delay = delay_ms_as_i64(&d.delay_ms)
        .ok_or_else(|| invalid_deadline("'delay_ms' must be a non-negative integer below 2**63"))?;
    Ok(Deadline { delay_ms: Value::from(delay), ..d.clone() })
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "text",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// Scalar fields of a stored record after [`check_record_fields`].
#[derive(Debug)]
pub(crate) struct RecordFields {
    pub(crate) snapshot: String,
    pub(crate) version: u64,
    pub(crate) machine_version: String,
    pub(crate) updated_at: f64,
}

/// Refuse a stored record whose scalar fields have the wrong type
/// (`check_record_fields` in Python): every failure is [`StoreError::Corrupt`].
pub(crate) fn check_record_fields(
    where_: &str,
    snapshot: &Value,
    version: &Value,
    machine_version: &Value,
    updated_at: &Value,
) -> Result<RecordFields> {
    let bad = |what: String| StoreError::Corrupt(format!("{where_}: {what}."));
    let Value::String(snapshot) = snapshot else {
        return Err(bad(format!("'snapshot' is {}, not text", kind_of(snapshot))));
    };
    let Some(version) = version.as_u64().filter(|v| *v >= 1) else {
        return Err(bad(format!("'version' {version} is not a positive integer")));
    };
    let Value::String(machine_version) = machine_version else {
        return Err(bad("'machine_version' is not text".to_owned()));
    };
    let Some(updated_at) = updated_at.as_f64().filter(|f| f.is_finite()) else {
        return Err(bad(format!("'updated_at' {updated_at} is not a finite number")));
    };
    Ok(RecordFields {
        snapshot: snapshot.clone(),
        version,
        machine_version: machine_version.clone(),
        updated_at,
    })
}

/// Validate and decode a deadline record loaded from storage (`check_deadline_record` +
/// `Deadline.from_dict`).
pub(crate) fn deadline_from_value(rec: &Value) -> std::result::Result<Deadline, String> {
    let Value::Object(map) = rec else {
        return Err(format!("expected an object, got {}", kind_of(rec)));
    };
    for key in ["state_id", "entry_seq", "due_at_wall", "delay_ms", "event_type"] {
        if !map.contains_key(key) {
            return Err(format!("missing key '{key}'"));
        }
    }
    let text = |key: &str| match &map[key] {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        _ => Err(format!("'{key}' must be a non-empty string")),
    };
    let state_id = text("state_id")?;
    let event_type = text("event_type")?;
    let entry_seq = map["entry_seq"]
        .as_u64()
        .filter(|n| *n < INT64_LIMIT)
        .ok_or_else(|| "'entry_seq' must be a non-negative integer below 2**63".to_owned())?;
    let delay = &map["delay_ms"];
    if delay.as_u64().is_none_or(|n| n >= INT64_LIMIT) {
        return Err("'delay_ms' must be a non-negative integer below 2**63".to_owned());
    }
    let due_at_wall = map["due_at_wall"].as_f64().filter(|f| f.is_finite()).ok_or_else(|| {
        "'due_at_wall' must be a finite number (seconds since the epoch)".to_owned()
    })?;
    Ok(Deadline { state_id, entry_seq, due_at_wall, delay_ms: delay.clone(), event_type })
}

// ---------------------------------------------------------------------------------------
// Per-key in-process locks
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Slot {
    owner: Mutex<Option<ThreadId>>,
    freed: Condvar,
}

/// A table of per-key, timed, non-reentrant locks that remember which thread holds each key.
#[derive(Debug, Default)]
pub(crate) struct KeyLocks {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

/// Holds one key of a [`KeyLocks`]; releases it on drop.
#[derive(Debug)]
pub(crate) struct KeyGuard {
    slot: Arc<Slot>,
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        *relock(&self.slot.owner) = None;
        self.slot.freed.notify_one();
    }
}

impl KeyLocks {
    fn slot(&self, key: &str) -> Arc<Slot> {
        Arc::clone(relock(&self.slots).entry(key.to_owned()).or_default())
    }

    /// Wait up to `timeout` for `key`; `None` when it stayed held.
    pub(crate) fn acquire(&self, key: &str, timeout: Duration) -> Option<KeyGuard> {
        let slot = self.slot(key);
        let deadline = deadline_after(timeout);
        let mut owner = relock(&slot.owner);
        while owner.is_some() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            owner =
                slot.freed.wait_timeout(owner, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
        *owner = Some(thread::current().id());
        drop(owner);
        Some(KeyGuard { slot })
    }

    /// `true` when the calling thread currently holds `key`.
    pub(crate) fn held_by_current(&self, key: &str) -> bool {
        let slot = relock(&self.slots).get(key).cloned();
        slot.is_some_and(|s| *relock(&s.owner) == Some(thread::current().id()))
    }

    /// Drop the bookkeeping for `key`, unless the lock is still held or being waited on: a
    /// removed slot would let a second holder in through a fresh one.
    pub(crate) fn forget(&self, key: &str) {
        let mut slots = relock(&self.slots);
        if slots.get(key).is_some_and(|slot| Arc::strong_count(slot) == 1) {
            slots.remove(key);
        }
    }
}

// ---------------------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Record {
    snapshot: String,
    version: u64,
    machine_version: String,
    updated_at: f64,
    deadlines: Vec<Deadline>,
}

/// Map-backed store for tests and single-process applications.
///
/// Thread-safe: one mutex guards the map and [`StateStore::lock`] hands out per-key locks with a
/// timeout. The data lives in this process only; use `FileStore` or `SqliteStore` for anything
/// that must survive a restart or be shared. It indexes deadlines, so
/// [`StateStore::due_keys`] is supported.
#[derive(Debug, Default)]
pub struct MemoryStore {
    records: Mutex<HashMap<String, Record>>,
    locks: KeyLocks,
    policy: StorePolicy,
}

impl MemoryStore {
    /// An empty store with the default 1 MiB snapshot cap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store that refuses snapshots larger than `max_snapshot_bytes` (at least 1).
    #[must_use]
    pub fn with_max_snapshot_bytes(max_snapshot_bytes: usize) -> Self {
        Self { policy: StorePolicy::with_max_snapshot_bytes(max_snapshot_bytes), ..Self::default() }
    }
}

#[derive(Debug)]
struct MemoryLockGuard {
    key: String,
    _guard: KeyGuard,
}

impl LockGuard for MemoryLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl StateStore for MemoryStore {
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        validate_key(key)?;
        let Some(rec) = relock(&self.records).get(key).cloned() else {
            return Ok(None);
        };
        self.policy.check_loaded(key, &rec.snapshot)?;
        Ok(Some(StoredSnapshot {
            key: key.to_owned(),
            snapshot: rec.snapshot,
            version: rec.version,
            machine_version: rec.machine_version,
            updated_at: rec.updated_at,
            deadlines: rec.deadlines,
        }))
    }

    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64> {
        let deadlines = self.policy.prepare_save(key, snapshot_json, options)?;
        let mut records = relock(&self.records);
        let existing = records.get(key).map(|r| r.version);
        let current = existing.unwrap_or(0);
        if let Some(expected) = options.expected_version.filter(|e| *e != current) {
            return Err(StoreError::Conflict { key: key.to_owned(), expected, actual: existing });
        }
        let version = current + 1;
        records.insert(
            key.to_owned(),
            Record {
                snapshot: snapshot_json.to_owned(),
                version,
                machine_version: options.machine_version.to_owned(),
                updated_at: wall_now(),
                deadlines,
            },
        );
        Ok(version)
    }

    fn delete(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        Ok(relock(&self.records).remove(key).is_some())
    }

    fn forget(&self, key: &str) -> Result<ForgetReport> {
        validate_key(key)?;
        let removed = relock(&self.records).remove(key);
        self.locks.forget(key);
        Ok(ForgetReport {
            snapshots: u64::from(removed.is_some()),
            deadlines: removed.map_or(0, |r| r.deadlines.len() as u64),
            other: Vec::new(),
        })
    }

    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let mut keys: Vec<String> =
            relock(&self.records).keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        keys.sort_unstable();
        keys.truncate(limit);
        keys.retain(|k| validate_key(k).is_ok());
        Ok(keys)
    }

    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        validate_key(key)?;
        let guard = self.locks.acquire(key, timeout).ok_or_else(|| StoreError::LockTimeout {
            key: key.to_owned(),
            timeout,
            holder: None,
        })?;
        Ok(Box::new(MemoryLockGuard { key: key.to_owned(), _guard: guard }))
    }

    fn health(&self) -> Result<Health> {
        let keys = relock(&self.records).len();
        Ok(Health {
            ok: true,
            backend: "memory",
            details: vec![("keys".to_owned(), keys.to_string())],
        })
    }

    fn due_keys(&self, until_wall: f64, limit: usize) -> Result<Vec<String>> {
        let mut due: Vec<(f64, String)> = relock(&self.records)
            .iter()
            .filter_map(|(k, r)| {
                let first = r.deadlines.iter().map(|d| d.due_at_wall).min_by(f64::total_cmp)?;
                (first <= until_wall).then(|| (first, k.clone()))
            })
            .collect();
        due.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(due.into_iter().take(limit).map(|(_, k)| k).collect())
    }

    fn max_snapshot_bytes(&self) -> usize {
        self.policy.max_snapshot_bytes()
    }
}
