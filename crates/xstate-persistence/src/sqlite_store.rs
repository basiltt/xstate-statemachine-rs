//! [`SqliteStore`]: a SQLite-backed store (WAL by default) with optimistic versioning and a
//! database-wide pessimistic lock.
//!
//! The schema is byte-compatible with Python's `SQLiteStore` (`persistence/sqlite_store.py`,
//! ADR-0003): schema version 2 in `xsm_schema`, tables `statecharts` and `deadlines`, the
//! `deadlines_due` / `deadlines_key` indexes and the v1 to v2 upgrade step, so one database file
//! can be shared by both runtimes.
//!
//! Connections: the store owns one connection behind a `Mutex` (SQLite itself serialises across
//! processes); [`StateStore::lock`] opens a *dedicated* connection that holds `BEGIN IMMEDIATE`
//! until the guard drops. While a thread holds that lock, its own `save` / `delete` calls are
//! routed through the locked connection (inside a `SAVEPOINT`) so they join the lock's
//! transaction instead of deadlocking against it, exactly like Python's per-thread connection.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use xstate_core::snapshot::Deadline;

use crate::memory_store::{
    KeyGuard, KeyLocks, StorePolicy, check_record_fields, deadline_from_value, relock,
};
use crate::store::Result;
use crate::store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, MAX_KEY_LEN, SaveOptions,
    StateStore, StoreError, StoredSnapshot, validate_key, wall_now,
};

/// Schema version written by this library. Bump together with an entry in `upgrade_steps`.
pub const SCHEMA_VERSION: i64 = 2;
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const JOURNAL_RETRY_SLEEP: Duration = Duration::from_millis(20);

// DDL copied from Python's `_CREATE_V1`, whitespace included (SQLite keeps the statement text).
const CREATE_V1: [&str; 5] = [
    "CREATE TABLE IF NOT EXISTS xsm_schema (\n        version INTEGER NOT NULL\n    )",
    "CREATE TABLE IF NOT EXISTS statecharts (\n        key             TEXT PRIMARY KEY,\n        snapshot        TEXT NOT NULL,\n        version         INTEGER NOT NULL,\n        machine_version TEXT NOT NULL DEFAULT '',\n        updated_at      REAL NOT NULL\n    )",
    "CREATE TABLE IF NOT EXISTS deadlines (\n        key         TEXT NOT NULL,\n        state_id    TEXT NOT NULL,\n        entry_seq   INTEGER NOT NULL,\n        due_at_wall REAL NOT NULL,\n        delay_ms    INTEGER NOT NULL,\n        event_type  TEXT NOT NULL,\n        FOREIGN KEY (key) REFERENCES statecharts(key) ON DELETE CASCADE\n    )",
    "CREATE INDEX IF NOT EXISTS deadlines_due ON deadlines(due_at_wall)",
    "CREATE INDEX IF NOT EXISTS deadlines_key ON deadlines(key)",
];
const STATECHARTS_COLUMNS: [&str; 5] =
    ["key", "snapshot", "version", "machine_version", "updated_at"];

/// Statements that bring schema `from` to `from + 1` (Python `_UPGRADES`).
fn upgrade_steps(from: i64) -> &'static [&'static str] {
    match from {
        1 => &["CREATE INDEX IF NOT EXISTS deadlines_key ON deadlines(key)"],
        _ => &[],
    }
}

/// SQLite `journal_mode` for a file database. Only the two modes the store supports (and
/// Python's `SQLiteStore` accepts) exist: the unsafe or in-memory modes are not offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JournalMode {
    /// Write-ahead log (default; readers do not block the writer).
    #[default]
    Wal,
    /// Rollback journal deleted after each transaction.
    Delete,
}

impl JournalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wal => "WAL",
            Self::Delete => "DELETE",
        }
    }
}

/// Tuning for [`SqliteStore::open_with`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SqliteStoreOptions {
    /// How long a connection waits on a busy database before failing (default 5 s). Must be
    /// representable as SQLite milliseconds (at most `i32::MAX` ms); larger values are refused
    /// by [`SqliteStore::open_with`].
    pub busy_timeout: Duration,
    /// Journal mode of a file database (default WAL; forced to `DELETE` on a UNC / network
    /// path). Ignored for `:memory:`.
    pub journal_mode: JournalMode,
    /// Snapshot size cap in bytes (default 1 MiB).
    pub max_snapshot_bytes: usize,
}

impl Default for SqliteStoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            journal_mode: JournalMode::Wal,
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }
}

impl SqliteStoreOptions {
    /// Set [`busy_timeout`](Self::busy_timeout).
    #[must_use]
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// Set [`journal_mode`](Self::journal_mode).
    #[must_use]
    pub fn journal_mode(mut self, mode: JournalMode) -> Self {
        self.journal_mode = mode;
        self
    }

    /// Set [`max_snapshot_bytes`](Self::max_snapshot_bytes).
    #[must_use]
    pub fn max_snapshot_bytes(mut self, max: usize) -> Self {
        self.max_snapshot_bytes = max;
        self
    }
}

// Errors

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(db_err)`
fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Unavailable(format!("SQLiteStore: {e}"))
}

/// Internal failure: a store-level error or a raw SQLite one still to be classified.
#[derive(Debug)]
enum Fail {
    Store(StoreError),
    Db(rusqlite::Error),
}

impl From<StoreError> for Fail {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

impl Fail {
    /// A busy / locked database is a retryable [`StoreError::LockTimeout`]; anything else
    /// SQLite reports is [`StoreError::Unavailable`] (never a leaked raw driver error).
    fn into_store(self, key: &str, timeout: Duration) -> StoreError {
        match self {
            Self::Store(e) => e,
            Self::Db(e) if is_busy(&e) => {
                StoreError::LockTimeout { key: key.to_owned(), timeout, holder: None }
            }
            Self::Db(e) => db_err(e),
        }
    }
}

type FailResult<T> = std::result::Result<T, Fail>;

// Connection setup and schema

/// A `Duration` is finite and non-negative by construction; the remaining invalid case is a
/// timeout SQLite cannot express (it takes `i32` milliseconds).
fn validate_busy_timeout(timeout: Duration) -> Result<()> {
    if timeout.as_millis() > u128::from(i32::MAX.unsigned_abs()) {
        return Err(StoreError::Other(format!(
            "SQLiteStoreOptions: busy_timeout {timeout:?} exceeds {} ms",
            i32::MAX
        )));
    }
    Ok(())
}

fn looks_like_network_path(path: &str) -> bool {
    path.starts_with("\\\\") || path.starts_with("//")
}

fn current_journal_mode(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
}

/// Switch a file database to `wanted`, retrying a lock for `busy` (concurrent openers race on
/// the switch; the loser re-reads the mode the winner set instead of failing to open).
fn ensure_journal_mode(conn: &Connection, busy: Duration, wanted: JournalMode) -> Result<()> {
    let mut current = current_journal_mode(conn).map_err(db_err)?;
    if current.eq_ignore_ascii_case(wanted.as_str()) {
        return Ok(());
    }
    let deadline = Instant::now() + busy;
    let statement = format!("PRAGMA journal_mode = {}", wanted.as_str());
    loop {
        match conn.query_row(&statement, [], |r| r.get::<_, String>(0)) {
            Ok(mode) => {
                current = mode;
                break;
            }
            Err(e) if is_busy(&e) => {}
            Err(e) => return Err(db_err(e)),
        }
        match current_journal_mode(conn) {
            Ok(mode) => current = mode,
            Err(e) if is_busy(&e) => {}
            Err(e) => return Err(db_err(e)),
        }
        if current.eq_ignore_ascii_case(wanted.as_str()) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(JOURNAL_RETRY_SLEEP);
    }
    if !current.eq_ignore_ascii_case(wanted.as_str()) {
        tracing::warn!(wanted = wanted.as_str(), current = %current, "could not switch SQLite journal_mode; continuing");
    }
    Ok(())
}

/// Open and configure one connection.
fn connect(path: &str, memory: bool, busy: Duration, journal: JournalMode) -> Result<Connection> {
    if memory {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON").map_err(db_err)?;
        return Ok(conn);
    }
    if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    precreate_private(path)?;
    // Not `Connection::open`: that would interpret a `file:` path as a URI.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(db_err)?;
    conn.busy_timeout(busy).map_err(db_err)?;
    conn.execute_batch("PRAGMA foreign_keys = ON").map_err(db_err)?;
    ensure_journal_mode(&conn, busy, journal)?;
    conn.execute_batch("PRAGMA synchronous = NORMAL").map_err(db_err)?;
    Ok(conn)
}

/// Create the database file `0600` before SQLite opens it, so it (and its `-wal` / `-shm`
/// siblings) are never world-readable.
#[cfg(unix)]
fn precreate_private(path: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if !Path::new(path).exists() {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn precreate_private(_path: &str) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::set_permissions(
            format!("{path}{suffix}"),
            std::fs::Permissions::from_mode(0o600),
        );
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &str) {}

/// Create the schema if needed, verify it is ours, and upgrade it to [`SCHEMA_VERSION`].
fn ensure_schema(conn: &mut Connection, path: &str) -> FailResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for statement in CREATE_V1 {
        tx.execute_batch(statement)?;
    }
    let columns: Vec<String> = tx
        .prepare("PRAGMA table_info(statecharts)")?
        .query_map([], |r| r.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !STATECHARTS_COLUMNS.iter().all(|c| columns.iter().any(|have| have == c)) {
        return Err(StoreError::Other(format!(
            "SQLiteStore({path}): table 'statecharts' exists with columns {columns:?}, not the xstate-statemachine \
             schema. Use another file."
        ))
        .into());
    }
    let stored: Option<i64> =
        tx.query_row("SELECT version FROM xsm_schema", [], |r| r.get(0)).optional()?;
    let mut current = SCHEMA_VERSION;
    if let Some(v) = stored {
        current = v;
    } else {
        tx.execute("INSERT INTO xsm_schema(version) VALUES (?1)", [SCHEMA_VERSION])?;
    }
    if current > SCHEMA_VERSION {
        return Err(StoreError::Corrupt(format!(
            "SQLiteStore schema is version {current}, newer than this library supports ({SCHEMA_VERSION}). Upgrade \
             xstate-persistence."
        ))
        .into());
    }
    while current < SCHEMA_VERSION {
        for statement in upgrade_steps(current) {
            tx.execute_batch(statement)?;
        }
        current += 1;
        tx.execute("UPDATE xsm_schema SET version = ?1", [current])?;
    }
    tx.commit()?;
    Ok(())
}

// Store

/// The connection pinned by an active [`StateStore::lock`], and the thread that holds it.
#[derive(Debug)]
struct Pinned {
    owner: ThreadId,
    conn: Connection,
}

#[derive(Debug)]
struct Inner {
    path: String,
    memory: bool,
    busy_timeout: Duration,
    journal_mode: JournalMode,
    policy: StorePolicy,
    main: Mutex<Connection>,
    pinned: Mutex<Option<Pinned>>,
    /// `:memory:` has a single connection, so its "database lock" is a process-level gate.
    memory_gate: KeyLocks,
}

/// SQLite-backed [`StateStore`]; see the [module docs](self) for the connection model.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    inner: Arc<Inner>,
}

impl SqliteStore {
    /// Open (creating if missing) a database file with default options.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] / [`StoreError::Io`] when the file cannot be opened,
    /// [`StoreError::Corrupt`] when its schema is newer than this library supports.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, SqliteStoreOptions::default())
    }

    /// Open a private in-memory database (one connection; for tests).
    ///
    /// # Errors
    /// As [`SqliteStore::open`].
    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// Open (creating if missing) a database file, or `:memory:`.
    ///
    /// # Errors
    /// As [`SqliteStore::open`].
    pub fn open_with(path: impl AsRef<Path>, options: SqliteStoreOptions) -> Result<Self> {
        validate_busy_timeout(options.busy_timeout)?;
        let path = path.as_ref().to_string_lossy().into_owned();
        let memory = path == ":memory:";
        let mut journal_mode = options.journal_mode;
        if !memory && looks_like_network_path(&path) {
            tracing::warn!(%path, "SQLite on a network path: locking is unreliable; using journal_mode=DELETE");
            journal_mode = JournalMode::Delete;
        }
        let mut conn = connect(&path, memory, options.busy_timeout, journal_mode)?;
        ensure_schema(&mut conn, &path).map_err(|f| f.into_store("<db>", options.busy_timeout))?;
        if !memory {
            restrict_permissions(&path);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                memory,
                busy_timeout: options.busy_timeout,
                journal_mode,
                policy: StorePolicy::with_max_snapshot_bytes(options.max_snapshot_bytes),
                main: Mutex::new(conn),
                pinned: Mutex::new(None),
                memory_gate: KeyLocks::default(),
            }),
        })
    }

    /// The database path (`:memory:` for an in-memory store).
    #[must_use]
    pub fn path(&self) -> &str {
        &self.inner.path
    }
}

/// Run `f` in a write transaction: `BEGIN IMMEDIATE`, or a `SAVEPOINT` when the connection is
/// already inside a lock's transaction (`joined`).
fn in_write_tx<T>(
    conn: &mut Connection,
    joined: bool,
    f: impl FnOnce(&Connection) -> FailResult<T>,
) -> FailResult<T> {
    if joined {
        let savepoint = conn.savepoint()?;
        let out = f(&savepoint)?;
        savepoint.commit()?;
        Ok(out)
    } else {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

/// Run `f` against one consistent read snapshot.
fn in_read_tx<T>(
    conn: &mut Connection,
    joined: bool,
    f: impl FnOnce(&Connection) -> FailResult<T>,
) -> FailResult<T> {
    if joined {
        return f(conn);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

impl Inner {
    /// Run `f` on this thread's connection: the lock-pinned one when this thread holds the
    /// database lock, otherwise the shared one.
    fn with_conn<T>(
        &self,
        key: &str,
        f: impl FnOnce(&mut Connection, bool) -> FailResult<T>,
    ) -> Result<T> {
        {
            let mut pinned = relock(&self.pinned);
            if let Some(p) = pinned.as_mut().filter(|p| p.owner == thread::current().id()) {
                return f(&mut p.conn, true).map_err(|e| e.into_store(key, self.busy_timeout));
            }
        }
        let mut conn = relock(&self.main);
        f(&mut conn, false).map_err(|e| e.into_store(key, self.busy_timeout))
    }
}

fn to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Integer(i) => Value::from(i),
        SqlValue::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        SqlValue::Text(s) => Value::String(s),
        // A BLOB or NULL where text / a number belongs is corruption; null fails every check.
        SqlValue::Null | SqlValue::Blob(_) => Value::Null,
    }
}

fn load_in(c: &Connection, key: &str) -> FailResult<Option<StoredSnapshot>> {
    let row = c
        .query_row(
            "SELECT snapshot, version, machine_version, updated_at FROM statecharts WHERE key = ?1",
            [key],
            |r| {
                Ok([to_json(r.get(0)?), to_json(r.get(1)?), to_json(r.get(2)?), to_json(r.get(3)?)])
            },
        )
        .optional()?;
    let Some([snapshot, version, machine_version, updated_at]) = row else { return Ok(None) };
    let fields = check_record_fields(
        &format!("SQLiteStore row for {key:?}"),
        &snapshot,
        &version,
        &machine_version,
        &updated_at,
    )?;
    let mut statement = c.prepare(
        "SELECT state_id, entry_seq, due_at_wall, delay_ms, event_type FROM deadlines WHERE key = ?1 \
         ORDER BY due_at_wall",
    )?;
    let rows = statement.query_map([key], |r| {
        Ok(serde_json::json!({
            "state_id": to_json(r.get(0)?),
            "entry_seq": to_json(r.get(1)?),
            "due_at_wall": to_json(r.get(2)?),
            "delay_ms": to_json(r.get(3)?),
            "event_type": to_json(r.get(4)?),
        }))
    })?;
    let mut deadlines = Vec::new();
    for rec in rows {
        let deadline = deadline_from_value(&rec?).map_err(|why| {
            StoreError::Corrupt(format!("SQLiteStore deadline row for {key:?}: {why}."))
        })?;
        deadlines.push(deadline);
    }
    Ok(Some(StoredSnapshot {
        key: key.to_owned(),
        snapshot: fields.snapshot,
        version: fields.version,
        machine_version: fields.machine_version,
        updated_at: fields.updated_at,
        deadlines,
    }))
}

fn save_in(
    c: &Connection,
    key: &str,
    snapshot_json: &str,
    options: &SaveOptions<'_>,
    deadlines: &[Deadline],
) -> FailResult<u64> {
    let existing: Option<i64> = c
        .query_row("SELECT version FROM statecharts WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    let existing = existing.map(|v| u64::try_from(v).unwrap_or(0));
    let current = existing.unwrap_or(0);
    let conflict = |actual| StoreError::Conflict {
        key: key.to_owned(),
        expected: options.expected_version.unwrap_or(0),
        actual,
    };
    if options.expected_version.is_some_and(|e| e != current) {
        return Err(conflict(existing).into());
    }
    let version = current + 1;
    let (new_i64, current_i64) = (
        i64::try_from(version).map_err(|_| StoreError::Other("version overflow".to_owned()))?,
        i64::try_from(current).unwrap_or(0),
    );
    let now = wall_now();
    if existing.is_none() {
        c.execute(
            "INSERT INTO statecharts(key, snapshot, version, machine_version, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, snapshot_json, new_i64, options.machine_version, now],
        )?;
    } else {
        // 🔒 The conditional UPDATE is the real optimistic check; the SELECT only shapes the error.
        let changed = c.execute(
            "UPDATE statecharts SET snapshot=?1, version=?2, machine_version=?3, updated_at=?4 \
             WHERE key=?5 AND version=?6",
            params![snapshot_json, new_i64, options.machine_version, now, key, current_i64],
        )?;
        if changed != 1 {
            return Err(conflict(None).into());
        }
    }
    c.execute("DELETE FROM deadlines WHERE key = ?1", [key])?;
    let mut insert = c.prepare(
        "INSERT INTO deadlines(key, state_id, entry_seq, due_at_wall, delay_ms, event_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for d in deadlines {
        insert.execute(params![
            key,
            d.state_id,
            i64::try_from(d.entry_seq).unwrap_or(i64::MAX),
            d.due_at_wall,
            d.delay_ms.as_i64().unwrap_or_default(),
            d.event_type
        ])?;
    }
    Ok(version)
}

fn forget_in(c: &Connection, key: &str) -> FailResult<ForgetReport> {
    let deadlines = c.execute("DELETE FROM deadlines WHERE key = ?1", [key])?;
    let snapshots = c.execute("DELETE FROM statecharts WHERE key = ?1", [key])?;
    // 🔐 a `SQLiteLog` sharing this file keys its rows by the store key; erase them too.
    let has_log: Option<i64> = c
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='transitions'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let log_entries = if has_log.is_some() {
        c.execute("DELETE FROM transitions WHERE machine_id = ?1", [key])?
    } else {
        0
    };
    Ok(ForgetReport {
        snapshots: snapshots as u64,
        deadlines: deadlines as u64,
        other: vec![("log_entries".to_owned(), log_entries as u64)],
    })
}

fn sql_count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

// Locks

/// Releases the database lock (commit, or rollback while panicking) when dropped.
#[derive(Debug)]
struct SqliteLockGuard {
    key: String,
    inner: Arc<Inner>,
}

impl LockGuard for SqliteLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for SqliteLockGuard {
    fn drop(&mut self) {
        // Clear the pin first: the next locker can only start once COMMIT below frees SQLite's
        // RESERVED lock, so it can never find the slot still occupied.
        let Some(pinned) = relock(&self.inner.pinned).take() else { return };
        let verb = if thread::panicking() { "ROLLBACK" } else { "COMMIT" };
        if let Err(e) = pinned.conn.execute_batch(verb) {
            tracing::warn!(key = %self.key, error = %e, "SQLite lock release failed; rolling back");
            let _ = pinned.conn.execute_batch("ROLLBACK");
        }
    }
}

/// Guard for a lock request made while this thread already holds the lock: joins it.
#[derive(Debug)]
struct NestedLockGuard {
    key: String,
}

impl LockGuard for NestedLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

/// Guard for the process-level gate used by `:memory:` stores.
#[derive(Debug)]
struct GateLockGuard {
    key: String,
    _gate: KeyGuard,
}

impl LockGuard for GateLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl SqliteStore {
    fn lock_memory(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        let gate = self.inner.memory_gate.acquire("", timeout).ok_or_else(|| {
            StoreError::LockTimeout { key: key.to_owned(), timeout, holder: None }
        })?;
        Ok(Box::new(GateLockGuard { key: key.to_owned(), _gate: gate }))
    }

    fn lock_file(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        let inner = &self.inner;
        let me = thread::current().id();
        if relock(&inner.pinned).as_ref().is_some_and(|p| p.owner == me) {
            return Ok(Box::new(NestedLockGuard { key: key.to_owned() }));
        }
        let conn = connect(&inner.path, false, timeout, inner.journal_mode)?;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| Fail::Db(e).into_store(key, timeout))?;
        conn.busy_timeout(inner.busy_timeout).map_err(db_err)?;
        let mut pinned = relock(&inner.pinned);
        if pinned.is_some() {
            drop(pinned);
            let _ = conn.execute_batch("ROLLBACK");
            return Err(StoreError::Other(
                "SQLiteStore lock slot unexpectedly occupied".to_owned(),
            ));
        }
        *pinned = Some(Pinned { owner: me, conn });
        Ok(Box::new(SqliteLockGuard { key: key.to_owned(), inner: Arc::clone(inner) }))
    }
}

// StateStore

impl StateStore for SqliteStore {
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        validate_key(key)?;
        let rec = self
            .inner
            .with_conn(key, |conn, joined| in_read_tx(conn, joined, |c| load_in(c, key)))?;
        if let Some(r) = &rec {
            self.inner.policy.check_loaded(key, &r.snapshot)?;
        }
        Ok(rec)
    }

    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64> {
        let deadlines = self.inner.policy.prepare_save(key, snapshot_json, options)?;
        self.inner.with_conn(key, |conn, joined| {
            in_write_tx(conn, joined, |c| save_in(c, key, snapshot_json, options, &deadlines))
        })
    }

    fn delete(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        self.inner.with_conn(key, |conn, joined| {
            in_write_tx(conn, joined, |c| {
                Ok(c.execute("DELETE FROM statecharts WHERE key = ?1", [key])? > 0)
            })
        })
    }

    fn forget(&self, key: &str) -> Result<ForgetReport> {
        validate_key(key)?;
        self.inner.with_conn(key, |conn, joined| in_write_tx(conn, joined, |c| forget_in(c, key)))
    }

    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut keys: Vec<String> = self.inner.with_conn("<db>", |conn, _| {
            // Rows a foreign writer planted under a key `load()` would refuse are filtered in
            // SQL, before `LIMIT`: otherwise an invalid key that sorts first (the empty string)
            // consumes a slot and starves the valid keys behind it.
            let mut statement = conn.prepare(
                "SELECT key FROM statecharts WHERE substr(key, 1, ?1) = ?2 \
                 AND key <> '' AND length(key) <= ?3 \
                 AND instr(CAST(key AS BLOB), X'00') = 0 \
                 ORDER BY key LIMIT ?4",
            )?;
            let rows = statement
                .query_map(
                    params![
                        sql_count(prefix.chars().count()),
                        prefix,
                        sql_count(MAX_KEY_LEN),
                        sql_count(limit)
                    ],
                    |r| r.get(0),
                )?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })?;
        keys.retain(|k| validate_key(k).is_ok());
        Ok(keys)
    }

    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        validate_key(key)?;
        if self.inner.memory {
            self.lock_memory(key, timeout)
        } else {
            self.lock_file(key, timeout)
        }
    }

    fn health(&self) -> Result<Health> {
        let probe = self.inner.with_conn("<db>", |conn, _| {
            let keys: i64 = conn.query_row("SELECT count(*) FROM statecharts", [], |r| r.get(0))?;
            Ok((keys, current_journal_mode(conn)?))
        });
        let mut details = vec![("path".to_owned(), self.inner.path.clone())];
        Ok(match probe {
            Ok((keys, journal)) => {
                details.push(("keys".to_owned(), keys.to_string()));
                details.push(("journal_mode".to_owned(), journal));
                details.push(("schema_version".to_owned(), SCHEMA_VERSION.to_string()));
                Health { ok: true, backend: "sqlite", details }
            }
            Err(e) => {
                details.push(("error".to_owned(), e.to_string()));
                Health { ok: false, backend: "sqlite", details }
            }
        })
    }

    fn due_keys(&self, until_wall: f64, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.inner.with_conn("<db>", |conn, _| {
            let mut statement = conn.prepare(
                "SELECT key, MIN(due_at_wall) AS first FROM deadlines WHERE due_at_wall <= ?1 \
                 GROUP BY key ORDER BY first, key LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![until_wall, sql_count(limit)], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
    }

    fn max_snapshot_bytes(&self) -> usize {
        self.inner.policy.max_snapshot_bytes()
    }
}
