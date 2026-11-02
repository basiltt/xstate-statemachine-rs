//! [`RedisStore`]: the snapshot store on Redis, in the layout Python's `RedisStore` writes.
//!
//! Python reference: `contrib/redis/store.py` and `_layout.py`. Every mutation is one Lua script
//! (atomic where a `WATCH` window would not be), so any number of Rust and Python processes may
//! share one namespace.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::conn::{Pool, error_label, scan_all};
use super::deadlines::{self, due_keys, parse_all};
use super::keys::{Keys, SCHEMA_VERSION, escape_glob};
use super::scripts::{DELETE, FORGET, PRUNE_KEYS, SAVE, SCHEMA, UNLOCK};
use crate::memory_store::{StorePolicy, check_record_fields, deadline_after};
use crate::store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, Result, SaveOptions, StateStore,
    StoreError, StoredSnapshot, validate_key, wall_now,
};

/// How long a `lock()` may be held before Redis expires it (Python `lock_ttl_ms`).
pub const DEFAULT_LOCK_TTL_MS: u64 = 30_000;

/// Pause between attempts to take a held lock.
const LOCK_POLL: Duration = Duration::from_millis(10);

/// Bound on releasing a lock from `Drop`: a Redis that went away must not stall the thread that
/// merely finished its guarded block (the lock then expires on its own after `lock_ttl_ms`).
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

/// Snapshot keys checked (and pruned if expired) per script call by `list_keys`.
const PRUNE_CHUNK: usize = 200;

/// Options of a [`RedisStore`].
#[derive(Debug, Clone)]
pub struct RedisStoreOptions {
    /// The key namespace: non-empty, no whitespace. Trailing colons are stripped. Two
    /// applications sharing one Redis must use different prefixes.
    pub prefix: String,
    /// Expiry of snapshot hashes (idle instances vanish); at least one millisecond.
    pub ttl_s: Option<f64>,
    /// How long a `lock()` may be held before Redis expires it, at least 1. This is the reason
    /// `persisted()` also fences with the version.
    pub lock_ttl_ms: u64,
    /// Size cap on save and on load.
    pub max_snapshot_bytes: usize,
}

impl RedisStoreOptions {
    /// Defaults under `prefix`: no TTL, a 30 s lock TTL and the 1 MiB snapshot cap.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            ttl_s: None,
            lock_ttl_ms: DEFAULT_LOCK_TTL_MS,
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }

    /// Expire snapshot hashes after `ttl_s` seconds.
    #[must_use]
    pub fn ttl_s(mut self, ttl_s: f64) -> Self {
        self.ttl_s = Some(ttl_s);
        self
    }

    /// Let a held lock expire after `lock_ttl_ms` milliseconds.
    #[must_use]
    pub fn lock_ttl_ms(mut self, lock_ttl_ms: u64) -> Self {
        self.lock_ttl_ms = lock_ttl_ms;
        self
    }

    /// Refuse snapshots larger than `max_snapshot_bytes`.
    #[must_use]
    pub fn max_snapshot_bytes(mut self, max_snapshot_bytes: usize) -> Self {
        self.max_snapshot_bytes = max_snapshot_bytes;
        self
    }
}

/// [`StateStore`] on Redis (blocking connections), byte-compatible with Python's `RedisStore`:
/// the same key names, hash fields, deadline index, Lua scripts and lock tokens, so one Redis
/// namespace can be shared by both runtimes.
///
/// Snapshots live in one hash per key (`snapshot`, `version`, `machine_version`, `updated_at`),
/// deadlines in a hash per key plus a sorted set scored by due time (which is what
/// [`StateStore::due_keys`] reads), locks in `SET NX PX` tokens released by compare-and-delete.
/// `forget` also erases the key's `RedisLog` stream. Not for a Redis you do not persist (AOF /
/// RDB) if you need durability across a Redis restart.
///
/// # Expiry and the key set
///
/// With [`RedisStoreOptions::ttl_s`] a snapshot hash expires on its own, but its member of the
/// `{p}:keys` set (what [`list_keys`](StateStore::list_keys) walks) does not: Redis cannot expire
/// one member of a set, and Python's store leaves it too. The member of an expired snapshot is
/// therefore retained until `list_keys` meets it: that call checks the keys it is about to
/// return and removes, atomically, the members whose snapshot is gone (so it never reports an
/// expired key, and the set shrinks as it is listed). `forget` and `delete` remove their key's
/// member at once.
#[derive(Debug)]
pub struct RedisStore {
    pool: Arc<Pool>,
    keys: Keys,
    ttl_ms: Option<u64>,
    lock_ttl_ms: u64,
    policy: StorePolicy,
}

impl RedisStore {
    /// Connect to `url` (`redis://host:6379/0`, `rediss://` for TLS) and check the namespace's
    /// schema marker, creating it when absent.
    ///
    /// # Errors
    /// An invalid URL, prefix or option; [`StoreError::Unavailable`] when Redis cannot be reached;
    /// [`StoreError::Other`] when the namespace's schema marker is newer than this library or
    /// unreadable.
    pub fn open(url: &str, options: &RedisStoreOptions) -> Result<Self> {
        let ttl_ms = ttl_millis(options.ttl_s)?;
        if options.lock_ttl_ms < 1 {
            return Err(StoreError::Other("lock_ttl_ms must be >= 1".into()));
        }
        let keys = Keys::new(&options.prefix)?;
        let pool = Arc::new(Pool::open(url)?);
        ensure_schema(&pool, &keys)?;
        Ok(Self {
            pool,
            keys,
            ttl_ms,
            lock_ttl_ms: options.lock_ttl_ms,
            policy: StorePolicy::with_max_snapshot_bytes(options.max_snapshot_bytes),
        })
    }

    /// The key namespace in use (trailing colons stripped).
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.keys.prefix()
    }
}

/// `ttl_s` in whole milliseconds: at least one, or the store would write records that are already
/// expired (a silent "save succeeded, load returns None").
fn ttl_millis(ttl_s: Option<f64>) -> Result<Option<u64>> {
    let Some(ttl) = ttl_s else { return Ok(None) };
    let ms = (ttl * 1000.0).trunc();
    if !ms.is_finite() || ms < 1.0 {
        return Err(StoreError::Other("ttl_s must be None or >= 0.001 (one millisecond)".into()));
    }
    // `ms` is finite and at least 1; the cast saturates and cannot wrap or go negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = ms as u64;
    Ok(Some(whole))
}

/// Validate a stored schema value; `true` when the marker must be (re)written.
fn schema_needs_write(prefix: &str, current: Option<&str>) -> Result<bool> {
    let Some(raw) = current else { return Ok(true) };
    let found: i64 = raw.trim().parse().map_err(|_| {
        StoreError::Other(format!(
            "Redis namespace '{prefix}' has an unreadable schema marker {raw:?}; refusing to use it."
        ))
    })?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::Other(format!(
            "Redis namespace '{prefix}' has schema version {found}, newer than this library \
             supports ({SCHEMA_VERSION})."
        )));
    }
    Ok(found < SCHEMA_VERSION)
}

/// Create the schema marker or raise an older one to ours; never lower a newer one.
pub(super) fn ensure_schema(pool: &Pool, keys: &Keys) -> Result<()> {
    let current: Option<String> = pool.run(|c| redis::cmd("GET").arg(keys.schema()).query(c))?;
    if schema_needs_write(keys.prefix(), current.as_deref())? {
        let marker: String =
            pool.run(|c| SCHEMA.key(keys.schema()).arg(SCHEMA_VERSION).invoke(c))?;
        schema_needs_write(keys.prefix(), Some(&marker))?;
    }
    Ok(())
}

fn count_at(res: &[i64], index: usize) -> u64 {
    res.get(index).copied().and_then(|n| u64::try_from(n).ok()).unwrap_or(0)
}

/// The new version from the save script's `{new, previous}` reply, or the conflict it reports.
fn saved_version(res: &[i64], key: &str, expected: Option<u64>) -> Result<u64> {
    let new = res.first().copied().unwrap_or(-1);
    if new == -1 {
        let actual = res.get(1).copied().and_then(|n| u64::try_from(n).ok());
        return Err(StoreError::Conflict {
            key: key.to_owned(),
            expected: expected.unwrap_or(0),
            actual,
        });
    }
    u64::try_from(new)
        .map_err(|_| StoreError::Other(format!("redis: save script returned version {new}")))
}

fn parsed<T: std::str::FromStr>(raw: &str, make: impl FnOnce(T) -> Value) -> Value {
    raw.trim().parse::<T>().map_or_else(|_| Value::String(raw.to_owned()), make)
}

/// Rebuild a record from its two hashes; a damaged one is [`StoreError::Corrupt`].
fn record(
    key: &str,
    hash: &HashMap<String, String>,
    deadlines: &HashMap<String, String>,
) -> Result<StoredSnapshot> {
    let snapshot = hash.get("snapshot").map_or(Value::Null, |s| Value::String(s.clone()));
    let version = hash.get("version").map_or(Value::Null, |r| parsed::<u64>(r, Value::from));
    let machine_version = Value::String(hash.get("machine_version").cloned().unwrap_or_default());
    let updated_at =
        hash.get("updated_at").map_or_else(|| Value::from(0.0), |r| parsed::<f64>(r, Value::from));
    let fields = check_record_fields(
        &format!("Redis record for '{key}'"),
        &snapshot,
        &version,
        &machine_version,
        &updated_at,
    )?;
    Ok(StoredSnapshot {
        key: key.to_owned(),
        snapshot: fields.snapshot,
        version: fields.version,
        machine_version: fields.machine_version,
        updated_at: fields.updated_at,
        deadlines: parse_all(deadlines),
    })
}

impl RedisStore {
    fn run_remove(
        &self,
        script: &'static redis::Script,
        key: &str,
        with_lock_and_log: bool,
    ) -> Result<Vec<i64>> {
        let k = &self.keys;
        self.pool.run(|conn| {
            let mut call = script.prepare_invoke();
            call.key(k.snap(key)).key(k.keys()).key(k.dl(key)).key(k.deadlines());
            if with_lock_and_log {
                call.key(k.lock(key)).key(k.log(key));
            }
            call.arg(key).invoke(conn)
        })
    }

    /// The first `limit` of the sorted `candidates` whose snapshot still exists. A snapshot
    /// hash that expired (`ttl_s`) leaves its member in the `{p}:keys` set; the members met on
    /// the way are removed, in one atomic script per chunk, so the set is pruned by the same
    /// calls that would otherwise advertise dead keys, at the cost of only the keys examined.
    fn live_keys(&self, set: &str, candidates: &[String], limit: usize) -> Result<Vec<String>> {
        let mut live: Vec<String> = Vec::new();
        for chunk in candidates.chunks(PRUNE_CHUNK) {
            let gone: Vec<String> = self.pool.run(|conn| {
                let mut call = PRUNE_KEYS.key(set);
                for key in chunk {
                    call.arg(key).arg(self.keys.snap(key));
                }
                call.invoke(conn)
            })?;
            live.extend(chunk.iter().filter(|k| !gone.contains(*k)).cloned());
            if live.len() >= limit {
                break;
            }
        }
        live.truncate(limit);
        Ok(live)
    }
}

impl StateStore for RedisStore {
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        validate_key(key)?;
        let (hash, deadlines): (HashMap<String, String>, HashMap<String, String>) =
            self.pool.run(|conn| {
                redis::pipe()
                    .atomic()
                    .cmd("HGETALL")
                    .arg(self.keys.snap(key))
                    .cmd("HGETALL")
                    .arg(self.keys.dl(key))
                    .query(conn)
            })?;
        if hash.is_empty() {
            return Ok(None);
        }
        let rec = record(key, &hash, &deadlines)?;
        self.policy.check_loaded(key, &rec.snapshot)?;
        Ok(Some(rec))
    }

    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64> {
        let deadlines = self.policy.prepare_save(key, snapshot_json, options)?;
        let k = &self.keys;
        let res: Vec<i64> = self.pool.run(|conn| {
            let mut call = SAVE.prepare_invoke();
            call.key(k.snap(key)).key(k.keys()).key(k.dl(key)).key(k.deadlines());
            call.arg(options.expected_version.map_or_else(String::new, |v| v.to_string()))
                .arg(snapshot_json)
                .arg(options.machine_version)
                .arg(format!("{:?}", wall_now()))
                .arg(key)
                .arg(self.ttl_ms.map_or_else(String::new, |ttl| ttl.to_string()));
            for arg in deadlines::script_args(key, &deadlines) {
                call.arg(arg);
            }
            call.invoke(conn)
        })?;
        saved_version(&res, key, options.expected_version)
    }

    fn delete(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        Ok(count_at(&self.run_remove(&DELETE, key, false)?, 0) > 0)
    }

    fn forget(&self, key: &str) -> Result<ForgetReport> {
        validate_key(key)?;
        let res = self.run_remove(&FORGET, key, true)?;
        Ok(ForgetReport {
            snapshots: count_at(&res, 0),
            deadlines: count_at(&res, 1),
            other: vec![
                ("locks".to_owned(), count_at(&res, 2)),
                ("log_entries".to_owned(), count_at(&res, 3)),
            ],
        })
    }

    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let pattern = format!("{}*", escape_glob(prefix));
        let set = self.keys.keys();
        let mut found = self.pool.run(|conn| scan_all(conn, Some(&set), &pattern))?;
        // A key planted by another writer that `load()` would refuse is never advertised, and it
        // must not use up the limit either: filter before truncating.
        found.retain(|k| k.starts_with(prefix) && validate_key(k).is_ok());
        self.live_keys(&set, &found, limit)
    }

    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        validate_key(key)?;
        let name = self.keys.lock(key);
        let token = new_token();
        let deadline = deadline_after(timeout);
        loop {
            let acquired: bool = self.pool.run(|conn| {
                let reply: Option<String> = redis::cmd("SET")
                    .arg(&name)
                    .arg(&token)
                    .arg("NX")
                    .arg("PX")
                    .arg(self.lock_ttl_ms)
                    .query(conn)?;
                Ok(reply.is_some())
            })?;
            if acquired {
                return Ok(Box::new(RedisLockGuard {
                    key: key.to_owned(),
                    name,
                    token,
                    pool: Arc::clone(&self.pool),
                }));
            }
            if std::time::Instant::now() >= deadline {
                return Err(StoreError::LockTimeout { key: key.to_owned(), timeout, holder: None });
            }
            thread::sleep(LOCK_POLL);
        }
    }

    fn health(&self) -> Result<Health> {
        let probe = self.pool.run(|conn| {
            let _: String = redis::cmd("PING").query(conn)?;
            redis::cmd("SCARD").arg(self.keys.keys()).query::<i64>(conn)
        });
        let mut details = Vec::new();
        Ok(match probe {
            Ok(count) => {
                details.push(("prefix".to_owned(), self.keys.prefix().to_owned()));
                details.push(("keys".to_owned(), count.to_string()));
                details.push(("schema_version".to_owned(), SCHEMA_VERSION.to_string()));
                Health { ok: true, backend: "redis", details }
            }
            // The error kind only: the message carries host:port.
            Err(e) => {
                details.push(("error".to_owned(), error_label(&e).to_owned()));
                Health { ok: false, backend: "redis", details }
            }
        })
    }

    fn due_keys(&self, until_wall: f64, limit: usize) -> Result<Vec<String>> {
        due_keys(&self.pool, &self.keys, until_wall, limit)
    }

    fn max_snapshot_bytes(&self) -> usize {
        self.policy.max_snapshot_bytes()
    }
}

/// 128 random bits as 32 hex digits (Python `secrets.token_hex(16)`).
fn new_token() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut bytes);
    hex::encode(bytes)
}

/// A held `SET NX PX` lock; dropping it releases it by compare-and-delete, so a guard whose
/// lock already expired (and was taken by someone else) cannot free the new owner's lock.
struct RedisLockGuard {
    key: String,
    name: String,
    token: String,
    pool: Arc<Pool>,
}

impl LockGuard for RedisLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for RedisLockGuard {
    fn drop(&mut self) {
        // Redis is reached over blocking sockets, so unlike an async client this never needs a
        // runtime and cannot panic inside one; the short bound keeps a vanished Redis from
        // stalling the caller. A failed release is logged, not returned: the lock then expires
        // after `lock_ttl_ms`, and whatever the guarded block returned is what the caller sees.
        let released = self.pool.run_within(RELEASE_TIMEOUT, |conn| {
            UNLOCK.key(&self.name).arg(&self.token).invoke::<i64>(conn)
        });
        if let Err(error) = released {
            tracing::warn!(key = %self.key, %error, "could not release the Redis lock");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_at_least_a_millisecond() {
        assert_eq!(ttl_millis(None).unwrap(), None);
        assert_eq!(ttl_millis(Some(0.5)).unwrap(), Some(500));
        assert_eq!(ttl_millis(Some(0.001)).unwrap(), Some(1));
        assert!(ttl_millis(Some(0.0001)).is_err());
        assert!(ttl_millis(Some(-1.0)).is_err());
        assert!(ttl_millis(Some(f64::NAN)).is_err());
    }

    #[test]
    fn schema_marker_rules() {
        assert!(schema_needs_write("p", None).unwrap());
        assert!(schema_needs_write("p", Some("1")).unwrap());
        assert!(!schema_needs_write("p", Some("2")).unwrap());
        assert!(schema_needs_write("p", Some("3")).is_err());
        assert!(schema_needs_write("p", Some("x")).is_err());
    }

    #[test]
    fn save_reply_maps_to_version_or_conflict() {
        assert_eq!(saved_version(&[3, 2], "k", None).unwrap(), 3);
        match saved_version(&[-1, 4], "k", Some(2)) {
            Err(StoreError::Conflict { expected: 2, actual: Some(4), .. }) => {}
            other => panic!("{other:?}"),
        }
        match saved_version(&[-1, -1], "k", Some(5)) {
            Err(StoreError::Conflict { expected: 5, actual: None, .. }) => {}
            other => panic!("{other:?}"),
        }
    }
}
