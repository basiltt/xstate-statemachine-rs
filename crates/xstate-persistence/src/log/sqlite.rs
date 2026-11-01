//! SQLite transition log (`transitions` table, the Python layout).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

use super::record::{TransitionLogStore, TransitionRecord, check_append, check_cutoff};
use crate::idempotency::sqlite_util::{open, open_memory, sql_err};
use crate::store::{Result, StoreError};

const CREATE_LOG: &str = "CREATE TABLE IF NOT EXISTS transitions (
    machine_id     TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    ts             REAL NOT NULL,
    record         TEXT NOT NULL,
    PRIMARY KEY (machine_id, seq)
)";
const CREATE_LOG_IDX: &str = "CREATE INDEX IF NOT EXISTS transitions_ts ON transitions(ts)";
const REQUIRED_COLUMNS: [&str; 4] = ["machine_id", "seq", "ts", "record"];

/// Log rows in a SQLite table, on the log's own connection. Safe across threads and
/// processes: [`append_next`](TransitionLogStore::append_next) reads `MAX(seq)` and inserts in
/// one `BEGIN IMMEDIATE` transaction, so concurrent writers on one key never mint the same
/// `seq`.
#[derive(Debug)]
pub struct SqliteLog {
    conn: Mutex<Connection>,
}

fn sql_seq(seq: u64) -> Result<i64> {
    i64::try_from(seq).map_err(|_| StoreError::Other(format!("seq {seq} exceeds the SQLite range")))
}

fn from_sql_seq(seq: i64) -> u64 {
    u64::try_from(seq).unwrap_or(0)
}

fn insert_err(record: &TransitionRecord, error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::ConstraintViolation =>
        {
            StoreError::Other(format!(
                "transition ({}, seq {}) already exists",
                record.machine_id, record.seq
            ))
        }
        _ => sql_err(error),
    }
}

impl SqliteLog {
    /// Open (creating if needed) a log database file.
    ///
    /// # Errors
    /// I/O or SQLite failure, or an existing `transitions` table with a foreign schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(open(path.as_ref())?)
    }

    /// A private in-memory log.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(open_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_err)?;
        tx.execute_batch(CREATE_LOG).map_err(sql_err)?;
        let columns: HashSet<String> = {
            let mut stmt = tx.prepare("PRAGMA table_info(transitions)").map_err(sql_err)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
            rows.collect::<std::result::Result<_, _>>().map_err(sql_err)?
        };
        if !REQUIRED_COLUMNS.iter().all(|c| columns.contains(*c)) {
            let mut found: Vec<&String> = columns.iter().collect();
            found.sort();
            return Err(StoreError::Other(format!(
                "SqliteLog: table 'transitions' exists with columns {found:?}, not the \
                 xstate-statemachine schema. Use another file."
            )));
        }
        tx.execute_batch(CREATE_LOG_IDX).map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(conn: &Connection, record: &TransitionRecord) -> Result<()> {
        conn.execute(
            "INSERT INTO transitions(machine_id, seq, ts, record) VALUES (?1, ?2, ?3, ?4)",
            params![record.machine_id, sql_seq(record.seq)?, record.ts, record.to_json()],
        )
        .map(|_| ())
        .map_err(|e| insert_err(record, e))
    }
}

impl TransitionLogStore for SqliteLog {
    fn append(&self, record: &TransitionRecord) -> Result<()> {
        check_append(record)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_err)?;
        Self::insert(&tx, record)?;
        tx.commit().map_err(sql_err)
    }

    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_err)?;
        let max: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM transitions WHERE machine_id = ?1",
                params![record.machine_id],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        let stored = TransitionRecord { seq: from_sql_seq(max) + 1, ..record };
        check_append(&stored)?;
        Self::insert(&tx, &stored)?;
        tx.commit().map_err(sql_err)?;
        Ok(stored)
    }

    fn next_seq(&self, machine_id: &str) -> Result<u64> {
        let max: i64 = self
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM transitions WHERE machine_id = ?1",
                params![machine_id],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        Ok(from_sql_seq(max) + 1)
    }

    fn read(
        &self,
        machine_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TransitionRecord>> {
        let conn = self.conn();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let after = i64::try_from(after_seq).unwrap_or(i64::MAX);
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, record FROM transitions \
                     WHERE machine_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(sql_err)?;
            let mapped = stmt
                .query_map(params![machine_id, after, limit], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql_err)?;
            mapped.collect::<std::result::Result<_, _>>().map_err(sql_err)?
        };
        if after_seq == 0 {
            // `seq > 0` never surfaces a non-positive seq: a silent hole in a replay.
            let hole: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM transitions WHERE machine_id = ?1 AND seq < 1 LIMIT 1",
                    params![machine_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if hole.is_some() {
                return Err(StoreError::Corrupt(format!(
                    "transitions({machine_id:?}): row with seq < 1"
                )));
            }
        }
        rows.into_iter()
            .map(|(seq, raw)| {
                let where_ = format!("transitions({machine_id:?}, seq={seq})");
                let record = TransitionRecord::from_json(&raw)
                    .map_err(|e| StoreError::Corrupt(format!("{where_}: {e}")))?;
                if i64::try_from(record.seq) != Ok(seq) || record.machine_id != machine_id {
                    return Err(StoreError::Corrupt(format!(
                        "{where_}: row key disagrees with its record ({:?}, {})",
                        record.machine_id, record.seq
                    )));
                }
                Ok(record)
            })
            .collect()
    }

    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64> {
        let cutoff = check_cutoff(cutoff_ts)?;
        self.conn()
            .execute("DELETE FROM transitions WHERE ts < ?1", params![cutoff])
            .map(|n| u64::try_from(n).unwrap_or(u64::MAX))
            .map_err(sql_err)
    }

    fn forget(&self, machine_id: &str) -> Result<u64> {
        self.conn()
            .execute("DELETE FROM transitions WHERE machine_id = ?1", params![machine_id])
            .map(|n| u64::try_from(n).unwrap_or(u64::MAX))
            .map_err(sql_err)
    }
}
