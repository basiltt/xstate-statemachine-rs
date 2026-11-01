//! The store contract: what every snapshot store implements and the policies applied around
//! it (key validation, size cap, codec seam), mirroring the Python `StateStore` protocol so a
//! database written by one runtime is readable by the other (ADR-0003).

use std::fmt;
use std::time::Duration;

use xstate_core::snapshot::Deadline;

/// Longest accepted store key (code points).
pub const MAX_KEY_LEN: usize = 200;
/// Largest accepted snapshot document in bytes (1 MiB), checked on save and on load.
pub const DEFAULT_MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

/// Errors raised by stores and the policies around them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// `expected_version` did not match the stored version; nothing was written.
    #[error("version conflict on '{key}': expected {expected:?}, found {actual:?}")]
    Conflict {
        /// Store key.
        key: String,
        /// Version the caller expected (`0` = must not exist).
        expected: u64,
        /// Version actually stored (`None` when the key does not exist).
        actual: Option<u64>,
    },
    /// The backend is unreachable or failed.
    #[error("store unavailable: {0}")]
    Unavailable(String),
    /// A lock could not be acquired in time.
    #[error("lock on '{key}' not acquired within {timeout:?}{}", holder.as_deref().map(|h| format!(" (held by {h})")).unwrap_or_default())]
    LockTimeout {
        /// Store key.
        key: String,
        /// Wait budget.
        timeout: Duration,
        /// Current holder, when known.
        holder: Option<String>,
    },
    /// The snapshot exceeds the size cap.
    #[error("snapshot for '{key}' is {size} bytes; limit is {limit}")]
    SnapshotTooLarge {
        /// Store key.
        key: String,
        /// Observed size.
        size: usize,
        /// Configured cap.
        limit: usize,
    },
    /// The key is empty, too long, or contains NUL.
    #[error("invalid store key: {0}")]
    InvalidKey(String),
    /// A stored record could not be decoded.
    #[error("stored snapshot is corrupt: {0}")]
    Corrupt(String),
    /// The key was not found where one was required.
    #[error("key '{0}' not found")]
    NotFound(String),
    /// Filesystem or database I/O failure.
    #[error("store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Error from the engine (snapshot restore, logic, …).
    #[error(transparent)]
    Engine(#[from] xstate_core::Error),
    /// Anything else, with context.
    #[error("{0}")]
    Other(String),
}

/// Result alias for store operations.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A stored record as returned by [`StateStore::load`].
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSnapshot {
    /// Store key.
    pub key: String,
    /// Snapshot JSON text (layout v4).
    pub snapshot: String,
    /// Record version: 1 on first save, +1 per save, restarts at 1 after `delete`.
    pub version: u64,
    /// Chart `version` label at save time (empty when the machine has none).
    pub machine_version: String,
    /// Store clock at save time, epoch seconds.
    pub updated_at: f64,
    /// Durable `after` deadlines indexed with the record.
    pub deadlines: Vec<Deadline>,
}

/// Options for [`StateStore::save`].
#[derive(Debug, Clone, Default)]
pub struct SaveOptions<'a> {
    /// `Some(n)`: write only if the stored version is `n` (`0` = the key must not exist);
    /// `None`: unconditional write.
    pub expected_version: Option<u64>,
    /// Chart `version` label to store alongside.
    pub machine_version: &'a str,
    /// Deadlines to index (lets [`StateStore::due_keys`] find the record).
    pub deadlines: &'a [Deadline],
}

/// Per-kind row counts removed by [`StateStore::forget`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetReport {
    /// Snapshot records removed (0 or 1).
    pub snapshots: u64,
    /// Deadline rows removed.
    pub deadlines: u64,
    /// Other rows removed (audit, inbox, …), by table name.
    pub other: Vec<(String, u64)>,
}

/// Backend health, for readiness probes.
#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    /// `true` when the backend answered.
    pub ok: bool,
    /// Backend name (`memory`, `file`, `sqlite`, …).
    pub backend: &'static str,
    /// Free-form details (path, schema version, …).
    pub details: Vec<(String, String)>,
}

/// Held lock on one key; released on drop.
pub trait LockGuard: Send {
    /// The locked key.
    fn key(&self) -> &str;
}

impl fmt::Debug for dyn LockGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LockGuard({})", self.key())
    }
}

/// A snapshot store. All methods are synchronous; wrap with `spawn_blocking` for async use.
///
/// Implementations validate keys with [`validate_key`] and enforce the size cap on save and
/// load. Versioning: the first save stores version 1, each save increments, `delete` resets.
pub trait StateStore: Send + Sync {
    /// Load a record, `None` when the key is unknown.
    ///
    /// # Errors
    /// Invalid key, corrupt record, oversized record, backend failure.
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>>;

    /// Save a snapshot; returns the new version.
    ///
    /// # Errors
    /// [`StoreError::Conflict`] when `expected_version` does not match; invalid key; snapshot
    /// over the cap; `machine_version` longer than 255 characters or containing NUL.
    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64>;

    /// Delete a record; `true` when something was removed.
    ///
    /// # Errors
    /// Invalid key or backend failure.
    fn delete(&self, key: &str) -> Result<bool>;

    /// Remove everything about a key (snapshot, deadlines, audit rows…): right to erasure.
    ///
    /// # Errors
    /// Invalid key or backend failure.
    fn forget(&self, key: &str) -> Result<ForgetReport>;

    /// Keys sorted by code point, optionally filtered by prefix, truncated at `limit`.
    ///
    /// # Errors
    /// Backend failure.
    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>>;

    /// Acquire a per-key lock (database-wide on SQLite), waiting up to `timeout`.
    ///
    /// # Errors
    /// [`StoreError::LockTimeout`] or backend failure.
    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>>;

    /// Backend health.
    ///
    /// # Errors
    /// Never for in-process stores; backend failure otherwise.
    fn health(&self) -> Result<Health>;

    /// Keys with at least one deadline due at or before `until_wall` (epoch seconds), earliest
    /// first, at most `limit`. Stores without a deadline index return
    /// [`StoreError::Other`]; the scanner then falls back to loading every record.
    ///
    /// # Errors
    /// Unsupported or backend failure.
    fn due_keys(&self, until_wall: f64, limit: usize) -> Result<Vec<String>> {
        let _ = (until_wall, limit);
        Err(StoreError::Other("this store has no deadline index".into()))
    }

    /// Largest snapshot this store accepts.
    fn max_snapshot_bytes(&self) -> usize {
        DEFAULT_MAX_SNAPSHOT_BYTES
    }
}

/// Validate a store key: non-empty, at most [`MAX_KEY_LEN`] code points, no NUL.
///
/// # Errors
/// [`StoreError::InvalidKey`] describing the violation.
pub fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(StoreError::InvalidKey("key must not be empty".into()));
    }
    let len = key.chars().count();
    if len > MAX_KEY_LEN {
        return Err(StoreError::InvalidKey(format!(
            "key has {len} characters; limit is {MAX_KEY_LEN}"
        )));
    }
    if key.contains('\0') {
        return Err(StoreError::InvalidKey("key must not contain NUL".into()));
    }
    Ok(())
}

/// Validate a chart version label: at most 255 characters, no NUL.
///
/// # Errors
/// [`StoreError::InvalidKey`] describing the violation.
pub fn validate_machine_version(version: &str) -> Result<()> {
    if version.chars().count() > 255 {
        return Err(StoreError::InvalidKey("machine_version longer than 255 characters".into()));
    }
    if version.contains('\0') {
        return Err(StoreError::InvalidKey("machine_version must not contain NUL".into()));
    }
    Ok(())
}

/// Enforce the size cap.
///
/// # Errors
/// [`StoreError::SnapshotTooLarge`].
pub fn check_size(key: &str, snapshot_json: &str, limit: usize) -> Result<()> {
    if snapshot_json.len() > limit {
        return Err(StoreError::SnapshotTooLarge {
            key: key.to_owned(),
            size: snapshot_json.len(),
            limit,
        });
    }
    Ok(())
}

/// Epoch seconds from the system clock.
#[must_use]
pub fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_rules() {
        assert!(validate_key("order:42").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("a\0b").is_err());
        assert!(validate_key(&"x".repeat(201)).is_err());
        assert!(validate_key(&"é".repeat(200)).is_ok(), "limit counts code points");
    }
}
