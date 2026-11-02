//! [`PostgresStore`]: the snapshot store on PostgreSQL, in the schema Python's `SQLAlchemyStore`
//! creates on its PostgreSQL dialect.
//!
//! Python reference: `contrib/sqlalchemy/store.py`, `_ops.py` and `_schema.py`.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::runtime::Handle;

use super::db::{Db, new_token, reject_nul, validate_ident};
use super::ops::{self, Table};
use super::schema::{self, DEFAULT_TABLE, SCHEMA_VERSION};
use crate::memory_store::{StorePolicy, deadline_after};
use crate::store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, Result, SaveOptions, StateStore,
    StoreError, StoredSnapshot, validate_key,
};

/// How long a `lock()` lease lives before another process may reclaim it (Python
/// `DEFAULT_LOCK_TTL_S`, 60 s). A crashed holder's lease expires on its own.
pub const DEFAULT_LOCK_TTL: Duration = Duration::from_secs(60);

/// Pause between attempts to take a held lease (Python `_LOCK_POLL_S`).
const LOCK_POLL: Duration = Duration::from_millis(10);
const DEFAULT_MAX_CONNECTIONS: u32 = 5;
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Tuning for [`PostgresStore::connect`] and [`PostgresStore::from_pool`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PostgresStoreOptions {
    /// Snapshot table (default `xsm_snapshots`). The auxiliary tables `xsm_deadlines`,
    /// `xsm_locks`, `xsm_inbox`, `xsm_transitions` and `xsm_schema` are shared by every store in
    /// the schema. Letters, digits and underscores only.
    pub table: String,
    /// Create the missing tables and record the schema version on open (default `true`,
    /// idempotent). `false` for databases whose DDL your migrations own: the schema version is
    /// still checked and missing tables are reported by name.
    pub create_tables: bool,
    /// `search_path` of every connection (`connect` only): the schema the tables live in. It must
    /// already exist. `None` uses the role's default.
    pub schema: Option<String>,
    /// Lifetime of a `lock()` lease (default [`DEFAULT_LOCK_TTL`]); must be positive.
    pub lock_ttl: Duration,
    /// Snapshot size cap in bytes (default 1 MiB), checked on save and on load.
    pub max_snapshot_bytes: usize,
    /// Pool size (`connect` only; default 5).
    pub max_connections: u32,
    /// How long an operation waits for a free pooled connection (`connect` only; default 10 s).
    pub acquire_timeout: Duration,
}

impl Default for PostgresStoreOptions {
    fn default() -> Self {
        Self {
            table: DEFAULT_TABLE.to_owned(),
            create_tables: true,
            schema: None,
            lock_ttl: DEFAULT_LOCK_TTL,
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        }
    }
}

impl PostgresStoreOptions {
    /// Use `table` as the snapshot table.
    #[must_use]
    pub fn table(mut self, table: impl Into<String>) -> Self {
        self.table = table.into();
        self
    }

    /// Create (`true`) or only verify (`false`) the tables on open.
    #[must_use]
    pub fn create_tables(mut self, create_tables: bool) -> Self {
        self.create_tables = create_tables;
        self
    }

    /// Put every connection on `search_path = schema`.
    #[must_use]
    pub fn schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Let a `lock()` lease expire after `lock_ttl`.
    #[must_use]
    pub fn lock_ttl(mut self, lock_ttl: Duration) -> Self {
        self.lock_ttl = lock_ttl;
        self
    }

    /// Refuse snapshots larger than `max_snapshot_bytes`.
    #[must_use]
    pub fn max_snapshot_bytes(mut self, max_snapshot_bytes: usize) -> Self {
        self.max_snapshot_bytes = max_snapshot_bytes;
        self
    }

    /// Size the pool.
    #[must_use]
    pub fn max_connections(mut self, max_connections: u32) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Bound the wait for a pooled connection.
    #[must_use]
    pub fn acquire_timeout(mut self, acquire_timeout: Duration) -> Self {
        self.acquire_timeout = acquire_timeout;
        self
    }
}

/// [`StateStore`] on PostgreSQL, in the tables Python's `SQLAlchemyStore` uses, so one database can
/// be shared by both runtimes: `xsm_snapshots` (or the configured table), `xsm_deadlines`,
/// `xsm_locks`, `xsm_inbox`, `xsm_transitions` and the `xsm_schema` version row.
///
/// Versioning is optimistic and happens in the statement itself (a conditional `UPDATE`, an
/// `INSERT .. ON CONFLICT`), never read-then-write. [`lock`](StateStore::lock) is the Python
/// lease: a row in `xsm_locks` that expires after [`PostgresStoreOptions::lock_ttl`]. Because a
/// lease can expire under a slow holder, `persisted()` also fences with the record version.
/// `forget` erases the record, its deadlines, its lease row and its `xsm_transitions` rows in one
/// transaction.
///
/// # Blocking, and where to call it from
///
/// The [`StateStore`] trait is synchronous and sqlx is not, so every call blocks the calling
/// thread on a runtime (`block_on`). **Do not call a method, drop a lock guard or drop the store
/// from inside an async task that runs on that same runtime**: from async code use
/// `tokio::task::spawn_blocking`, like for the other stores.
///
/// * [`connect`](Self::connect) builds its own small multi-thread runtime and pool; use it from
///   ordinary threads (or `spawn_blocking`).
/// * [`from_pool`](Self::from_pool) wraps a pool you already run, driven by your runtime's
///   [`Handle`], for applications that are tokio-based. Call it (and the store) from
///   `spawn_blocking` or a plain thread, never from a task; the runtime must be multi-threaded.
///
/// The `schema` option of `connect` sets `search_path`; with `from_pool`, configure the pool's
/// connections yourself.
#[derive(Debug)]
pub struct PostgresStore {
    db: Arc<Db>,
    table: Table,
    lock_ttl: Duration,
    policy: StorePolicy,
}

impl PostgresStore {
    /// Connect to `url` (`postgres://user:pass@host:5432/db`) on a runtime of this store's own,
    /// then create or verify the schema as [`PostgresStoreOptions::create_tables`] says.
    ///
    /// # Errors
    /// An invalid URL or option; [`StoreError::Unavailable`] when the server cannot be reached;
    /// [`StoreError::Corrupt`] when the schema version is newer than this library supports;
    /// [`StoreError::Other`] when `create_tables` is `false` and tables are missing.
    pub fn connect(url: &str, options: &PostgresStoreOptions) -> Result<Self> {
        check_options(options)?;
        let db = Db::connect(
            url,
            options.schema.as_deref(),
            options.max_connections,
            options.acquire_timeout,
        )?;
        Self::open(db, options)
    }

    /// Wrap an existing sqlx `pool` driven by the runtime behind `handle`, then create or verify
    /// the schema. `options.schema`, `max_connections` and `acquire_timeout` do not apply (they
    /// configure a pool, and the pool is yours).
    ///
    /// # Errors
    /// As [`connect`](Self::connect), minus the URL.
    pub fn from_pool(pool: PgPool, handle: Handle, options: &PostgresStoreOptions) -> Result<Self> {
        check_options(options)?;
        Self::open(Db::from_pool(pool, handle), options)
    }

    fn open(db: Db, options: &PostgresStoreOptions) -> Result<Self> {
        let table = Table {
            raw: options.table.clone(),
            sql: format!("\"{}\"", validate_ident("table", &options.table)?),
        };
        db.run("<db>", async {
            if options.create_tables {
                schema::ensure_schema(db.pool(), &table.raw, &table.sql).await
            } else {
                schema::check_schema(db.pool(), &table.raw, &table.sql).await
            }
        })?;
        Ok(Self {
            db: Arc::new(db),
            table,
            lock_ttl: options.lock_ttl,
            policy: StorePolicy::with_max_snapshot_bytes(options.max_snapshot_bytes),
        })
    }

    /// The snapshot table in use.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table.raw
    }

    /// The connection and runtime, for the inbox and log that share this store's database.
    pub(super) fn db(&self) -> &Arc<Db> {
        &self.db
    }
}

fn check_options(options: &PostgresStoreOptions) -> Result<()> {
    if options.lock_ttl.is_zero() {
        return Err(StoreError::Other("lock_ttl must be > 0".into()));
    }
    validate_ident("table", &options.table)?;
    if let Some(schema) = &options.schema {
        validate_ident("schema", schema)?;
    }
    Ok(())
}

impl StateStore for PostgresStore {
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        validate_key(key)?;
        let rec = self.db.run(key, ops::load(self.db.pool(), &self.table, key))?;
        if let Some(r) = &rec {
            self.policy.check_loaded(key, &r.snapshot)?;
        }
        Ok(rec)
    }

    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64> {
        let deadlines = self.policy.prepare_save(key, snapshot_json, options)?;
        reject_nul("snapshot", snapshot_json)?;
        self.db.run(
            key,
            ops::save(self.db.pool(), &self.table, key, snapshot_json, options, &deadlines),
        )
    }

    fn delete(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        self.db.run(key, ops::delete(self.db.pool(), &self.table, key))
    }

    fn forget(&self, key: &str) -> Result<ForgetReport> {
        validate_key(key)?;
        self.db.run(key, ops::forget(self.db.pool(), &self.table, key))
    }

    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        // No stored key contains NUL, and PostgreSQL cannot be asked about one.
        if limit == 0 || prefix.contains('\0') {
            return Ok(Vec::new());
        }
        let mut keys =
            self.db.run("<db>", ops::list_keys(self.db.pool(), &self.table, prefix, limit))?;
        // A row another writer planted under a key `load()` would refuse is never advertised.
        keys.retain(|k| validate_key(k).is_ok());
        Ok(keys)
    }

    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        validate_key(key)?;
        let owner = new_token();
        let deadline = deadline_after(timeout);
        loop {
            let attempt = self.db.run(
                key,
                ops::try_lock(
                    self.db.pool(),
                    &self.table,
                    key,
                    &owner,
                    self.lock_ttl.as_secs_f64(),
                ),
            );
            match attempt {
                Ok(true) => {
                    return Ok(Box::new(PostgresLockGuard {
                        key: key.to_owned(),
                        source: self.table.raw.clone(),
                        owner,
                        db: Arc::clone(&self.db),
                    }));
                }
                // Contention (lock_timeout, deadlock) is "not yet", like a held lease.
                Ok(false) | Err(StoreError::LockTimeout { .. }) => {}
                Err(other) => return Err(other),
            }
            if Instant::now() >= deadline {
                return Err(StoreError::LockTimeout { key: key.to_owned(), timeout, holder: None });
            }
            thread::sleep(LOCK_POLL);
        }
    }

    fn health(&self) -> Result<Health> {
        let probe = self.db.run_raw(ops::count_keys(self.db.pool(), &self.table));
        let mut details = vec![
            ("table".to_owned(), self.table.raw.clone()),
            ("dialect".to_owned(), "postgresql".to_owned()),
        ];
        Ok(match probe {
            Ok(keys) => {
                details.push(("keys".to_owned(), keys.to_string()));
                details.push(("schema_version".to_owned(), SCHEMA_VERSION.to_string()));
                Health { ok: true, backend: "postgres", details }
            }
            // The failure kind only: the message can carry the host and the statement.
            Err(e) => {
                details.push(("error".to_owned(), e.label().to_owned()));
                Health { ok: false, backend: "postgres", details }
            }
        })
    }

    fn due_keys(&self, until_wall: f64, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.db.run("<db>", ops::due_keys(self.db.pool(), &self.table, until_wall, limit))
    }

    fn max_snapshot_bytes(&self) -> usize {
        self.policy.max_snapshot_bytes()
    }
}

/// A held lease row; dropping it deletes the row (only while this guard's token still owns it, so a
/// guard whose lease expired and was taken by someone else cannot free the new owner's lease).
#[derive(Debug)]
struct PostgresLockGuard {
    key: String,
    source: String,
    owner: String,
    db: Arc<Db>,
}

impl LockGuard for PostgresLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for PostgresLockGuard {
    fn drop(&mut self) {
        let released = self
            .db
            .run(&self.key, ops::unlock(self.db.pool(), &self.source, &self.key, &self.owner));
        if let Err(error) = released {
            // The lease then expires on its own after `lock_ttl`; the guarded block's result is
            // what the caller must see.
            tracing::warn!(key = %self.key, %error, "could not release the PostgreSQL lock lease");
        }
    }
}
