//! Connection plumbing shared by the SQLite outbox and dead-letter stores.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{Result, sql_err};

/// Seconds a connection waits on a busy database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a database file the way the Python stores do: the `SQLiteStore` schema is created (or
/// verified) first, because Python's `SQLiteOutboxStore(path)` / `SQLiteDeadLetterStore(path)`
/// open a `SQLiteStore` on it; then a connection of our own (busy timeout, WAL, mode 0600 via
/// the store).
pub(crate) fn open(path: &Path) -> Result<Connection> {
    drop(xstate_persistence::SqliteStore::open(path)?);
    let conn = Connection::open(path).map_err(sql_err)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)?;
    conn.execute_batch("PRAGMA synchronous = NORMAL").map_err(sql_err)?;
    Ok(conn)
}

/// A private in-memory database.
pub(crate) fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().map_err(sql_err)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)?;
    Ok(conn)
}

/// Run `body` in a `BEGIN IMMEDIATE` transaction, committing on `Ok`.
pub(crate) fn immediate<T>(
    conn: &mut Connection,
    body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let out = body(&tx)?;
    tx.commit().map_err(sql_err)?;
    Ok(out)
}

/// `?, ?, ?` for `n` parameters.
pub(crate) fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}
