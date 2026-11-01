//! SQLite-backed inbox (the Python `inbox` table) and the connection plumbing it shares with
//! the transition log.

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{InboxEntry, InboxStore, count, expiry, is_live};
use crate::store::{Result, wall_now};

/// Connection plumbing shared by the SQLite-backed inbox and log.
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite_util {
    use std::path::Path;
    use std::time::Duration;

    use rusqlite::Connection;

    use crate::store::{Result, StoreError};

    /// Seconds a connection waits on a busy database.
    const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

    /// Map a SQLite failure to the store error taxonomy.
    #[allow(clippy::needless_pass_by_value)] // used as a `map_err` adapter, which passes by value
    pub(crate) fn sql_err(error: rusqlite::Error) -> StoreError {
        StoreError::Unavailable(format!("sqlite: {error}"))
    }

    /// Open (creating, mode 0600 on Unix) a database file: busy timeout, WAL, normal sync.
    pub(crate) fn open(path: &Path) -> Result<Connection> {
        if !path.exists() {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options.open(path)?;
        }
        let conn = Connection::open(path).map_err(sql_err)?;
        configure(&conn)?;
        let mode: String =
            conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).map_err(sql_err)?;
        if !mode.eq_ignore_ascii_case("wal") {
            // Switching needs an exclusive lock; a busy database keeps the mode it has.
            let _ = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0));
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL").map_err(sql_err)?;
        Ok(conn)
    }

    /// Open a private in-memory database.
    pub(crate) fn open_memory() -> Result<Connection> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        configure(&conn)?;
        Ok(conn)
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)
    }
}

#[cfg(feature = "sqlite")]
const CREATE_INBOX: &str = "CREATE TABLE IF NOT EXISTS inbox (
    scope       TEXT NOT NULL,
    key         TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    receipt     TEXT,
    expires_at  REAL,
    PRIMARY KEY (scope, key)
)";
#[cfg(feature = "sqlite")]
const CREATE_INBOX_IDX: &str = "CREATE INDEX IF NOT EXISTS inbox_exp ON inbox(expires_at)";

/// Inbox in a SQLite table (`inbox`, the Python layout), on its own connection. Safe across
/// threads (the connection is serialised) and across processes (`claim` runs in a
/// `BEGIN IMMEDIATE` transaction).
#[cfg(feature = "sqlite")]
#[derive(Debug)]
pub struct SqliteInbox {
    conn: Mutex<rusqlite::Connection>,
}

#[cfg(feature = "sqlite")]
impl SqliteInbox {
    /// Open (creating if needed) an inbox database file.
    ///
    /// # Errors
    /// I/O or SQLite failure.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::init(sqlite_util::open(path.as_ref())?)
    }

    /// A private in-memory inbox.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(sqlite_util::open_memory()?)
    }

    fn init(mut conn: rusqlite::Connection) -> Result<Self> {
        use sqlite_util::sql_err;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        tx.execute_batch(CREATE_INBOX).map_err(sql_err)?;
        tx.execute_batch(CREATE_INBOX_IDX).map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(feature = "sqlite")]
impl InboxStore for SqliteInbox {
    fn get(&self, scope: &str, key: &str) -> Result<Option<InboxEntry>> {
        use rusqlite::OptionalExtension;
        self.conn()
            .query_row(
                "SELECT fingerprint, receipt, expires_at FROM inbox \
                 WHERE scope=?1 AND key=?2 AND (expires_at IS NULL OR expires_at > ?3)",
                rusqlite::params![scope, key, wall_now()],
                |row| {
                    Ok(InboxEntry {
                        fingerprint: row.get(0)?,
                        receipt_json: row.get(1)?,
                        expires_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(sqlite_util::sql_err)
    }

    fn claim(&self, scope: &str, key: &str, fingerprint: &str, ttl_s: Option<f64>) -> Result<bool> {
        use rusqlite::OptionalExtension;
        use sqlite_util::sql_err;
        let now = wall_now();
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let existing: Option<Option<f64>> = tx
            .query_row(
                "SELECT expires_at FROM inbox WHERE scope=?1 AND key=?2",
                rusqlite::params![scope, key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if existing.is_some_and(|expires_at| is_live(expires_at, now)) {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO inbox(scope, key, fingerprint, receipt, expires_at) \
             VALUES (?1, ?2, ?3, NULL, ?4)",
            rusqlite::params![scope, key, fingerprint, expiry(ttl_s, now)],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    fn mark(&self, scope: &str, key: &str, receipt_json: &str, ttl_s: Option<f64>) -> Result<()> {
        self.conn()
            .execute(
                "UPDATE inbox SET receipt=?1, expires_at=?2 WHERE scope=?3 AND key=?4",
                rusqlite::params![receipt_json, expiry(ttl_s, wall_now()), scope, key],
            )
            .map(|_| ())
            .map_err(sqlite_util::sql_err)
    }

    fn release(&self, scope: &str, key: &str) -> Result<()> {
        self.conn()
            .execute(
                "DELETE FROM inbox WHERE scope=?1 AND key=?2 AND receipt IS NULL",
                rusqlite::params![scope, key],
            )
            .map(|_| ())
            .map_err(sqlite_util::sql_err)
    }

    fn purge_expired(&self, now: Option<f64>) -> Result<u64> {
        self.conn()
            .execute(
                "DELETE FROM inbox WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                rusqlite::params![now.unwrap_or_else(wall_now)],
            )
            .map(count)
            .map_err(sqlite_util::sql_err)
    }

    fn forget(&self, scope: &str) -> Result<u64> {
        self.conn()
            .execute("DELETE FROM inbox WHERE scope=?1", rusqlite::params![scope])
            .map(count)
            .map_err(sqlite_util::sql_err)
    }
}
