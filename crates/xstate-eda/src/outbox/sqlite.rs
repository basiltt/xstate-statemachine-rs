//! [`SqliteOutboxStore`]: the Python `xsm_outbox` table, so a Python relay can drain rows a
//! Rust machine wrote and vice versa.

use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, params, params_from_iter};

use super::{OutboxRecord, OutboxStore};
use crate::envelope::{DEFAULT_MAX_SNAPSHOT_BYTES, Envelope};
use crate::error::{Result, sql_err};
use crate::py::{WallClock, system_clock};
use crate::sqlite_util::{self, marks};

const CREATE_OUTBOX: &str = "
CREATE TABLE IF NOT EXISTS xsm_outbox (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL,
    topic         TEXT NOT NULL,
    subject       TEXT,
    envelope      TEXT NOT NULL,
    created_at    REAL NOT NULL,
    sent_at       REAL,
    claimed_by    TEXT,
    claimed_until REAL
)
";
const CREATE_OUTBOX_IDX: &str =
    "CREATE INDEX IF NOT EXISTS xsm_outbox_pending ON xsm_outbox(sent_at, seq)";
/// Tables created by Python 0.11.0 lack the lease columns.
const OUTBOX_UPGRADES: [(&str, &str); 2] = [
    ("claimed_by", "ALTER TABLE xsm_outbox ADD COLUMN claimed_by TEXT"),
    ("claimed_until", "ALTER TABLE xsm_outbox ADD COLUMN claimed_until REAL"),
];

/// Lease owner that parks an undecodable row so no relay claims it again (Python
/// `POISON_OWNER`).
pub const POISON_OWNER: &str = "xsm:poison";
/// The parked rows' lease expiry (Python `_POISON_UNTIL`): never.
const POISON_UNTIL: f64 = 1e18;

/// Outbox in the `xsm_outbox` table, on its own connection.
///
/// Python's store can share the `SQLiteStore` connection so rows commit with the snapshot;
/// that shared-transaction mode is not ported (as for the idempotency inbox). Write rows from
/// the `persisted()` body or an `after_commit` callback (see [`OutboxPlugin`](super::OutboxPlugin)).
pub struct SqliteOutboxStore {
    conn: Mutex<Connection>,
    clock: WallClock,
}

impl std::fmt::Debug for SqliteOutboxStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteOutboxStore").finish_non_exhaustive()
    }
}

impl SqliteOutboxStore {
    /// Open (creating if needed) a database file, with the `SQLiteStore` schema Python creates
    /// alongside; old tables gain the lease columns.
    ///
    /// # Errors
    /// I/O or SQLite failure.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::init(sqlite_util::open(path.as_ref())?)
    }

    /// A private in-memory outbox.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(sqlite_util::open_memory()?)
    }

    /// Use `clock` (epoch seconds) for timestamps and leases.
    #[must_use]
    pub fn with_clock(mut self, clock: WallClock) -> Self {
        self.clock = clock;
        self
    }

    fn init(mut conn: Connection) -> Result<Self> {
        sqlite_util::immediate(&mut conn, |tx| {
            tx.execute_batch(CREATE_OUTBOX).map_err(sql_err)?;
            tx.execute_batch(CREATE_OUTBOX_IDX).map_err(sql_err)?;
            let mut stmt = tx.prepare("PRAGMA table_info(xsm_outbox)").map_err(sql_err)?;
            let have: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql_err)?;
            for (column, ddl) in OUTBOX_UPGRADES {
                if !have.iter().any(|c| c == column) {
                    tx.execute_batch(ddl).map_err(sql_err)?;
                }
            }
            Ok(())
        })?;
        Ok(Self { conn: Mutex::new(conn), clock: system_clock() })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decode rows one by one; a corrupt row is logged and skipped, never fatal (Python
    /// `_decode`). With `quarantine` (a claim) the bad rows are parked under
    /// [`POISON_OWNER`] so no relay selects them again; they stay unsent in the table for an
    /// operator to inspect.
    fn decode(
        &self,
        rows: Vec<(i64, String, String)>,
        quarantine: bool,
    ) -> Result<Vec<OutboxRecord>> {
        let mut out = Vec::with_capacity(rows.len());
        let mut bad = Vec::new();
        for (seq, topic, text) in rows {
            match Envelope::from_json(&text, DEFAULT_MAX_SNAPSHOT_BYTES) {
                Ok(envelope) => out.push(OutboxRecord { seq, topic, envelope }),
                Err(e) => {
                    tracing::error!(target: "xstate::eda", seq, error = %e, "outbox row is undecodable; skipped");
                    bad.push(seq);
                }
            }
        }
        if quarantine && !bad.is_empty() {
            self.park(&bad)?;
        }
        Ok(out)
    }

    fn park(&self, seqs: &[i64]) -> Result<()> {
        let sql = format!(
            "UPDATE xsm_outbox SET claimed_by = ?, claimed_until = ? WHERE seq IN ({})",
            marks(seqs.len())
        );
        let mut values: Vec<rusqlite::types::Value> =
            vec![POISON_OWNER.to_owned().into(), POISON_UNTIL.into()];
        values.extend(seqs.iter().map(|s| rusqlite::types::Value::Integer(*s)));
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(&sql, params_from_iter(values)).map(|_| ()).map_err(sql_err)
        })
    }

    /// Delete relayed rows older than `older_than_s` seconds (retention); returns how many.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn purge_sent(&self, older_than_s: f64) -> Result<usize> {
        let cutoff = (self.clock)() - older_than_s;
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(
                "DELETE FROM xsm_outbox WHERE sent_at IS NOT NULL AND sent_at < ?1",
                [cutoff],
            )
            .map_err(sql_err)
        })
    }

    /// All rows, or only unsent ones.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn count(&self, pending_only: bool) -> Result<usize> {
        let sql = if pending_only {
            "SELECT count(*) FROM xsm_outbox WHERE sent_at IS NULL"
        } else {
            "SELECT count(*) FROM xsm_outbox"
        };
        let n: i64 = self.conn().query_row(sql, [], |r| r.get(0)).map_err(sql_err)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

fn limit_param(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

impl OutboxStore for SqliteOutboxStore {
    fn add(&self, topic: &str, envelope: &Envelope) -> Result<()> {
        let text = envelope.to_json(DEFAULT_MAX_SNAPSHOT_BYTES)?;
        let now = (self.clock)();
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(
                "INSERT INTO xsm_outbox(id, topic, subject, envelope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![envelope.id, topic, envelope.subject, text, now],
            )
            .map(|_| ())
            .map_err(sql_err)
        })
    }

    fn pending(&self, limit: usize) -> Result<Vec<OutboxRecord>> {
        let rows = {
            let conn = self.conn();
            let mut stmt = conn
                .prepare("SELECT seq, topic, envelope FROM xsm_outbox WHERE sent_at IS NULL ORDER BY seq LIMIT ?1")
                .map_err(sql_err)?;
            stmt.query_map([limit_param(limit)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?
        };
        self.decode(rows, false)
    }

    fn mark_sent(&self, seqs: &[i64]) -> Result<usize> {
        if seqs.is_empty() {
            return Ok(0);
        }
        let now = (self.clock)();
        sqlite_util::immediate(&mut self.conn(), |tx| {
            let mut n = 0;
            for s in seqs {
                n += tx
                    .execute(
                        "UPDATE xsm_outbox SET sent_at = ?1 WHERE seq = ?2 AND sent_at IS NULL",
                        params![now, s],
                    )
                    .map_err(sql_err)?;
            }
            Ok(n)
        })
    }

    fn claim(&self, limit: usize, owner: &str, lease_s: f64) -> Result<Option<Vec<OutboxRecord>>> {
        let now = (self.clock)();
        let rows: Vec<(i64, String, String)> = sqlite_util::immediate(&mut self.conn(), |tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT seq, topic, envelope FROM xsm_outbox WHERE sent_at IS NULL AND \
                     (claimed_until IS NULL OR claimed_until <= ?1 OR claimed_by = ?2) \
                     ORDER BY seq LIMIT ?3",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![now, owner, limit_param(limit)], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<(i64, String, String)>>>()
                .map_err(sql_err)?;
            if !rows.is_empty() {
                let sql = format!(
                    "UPDATE xsm_outbox SET claimed_by = ?, claimed_until = ? WHERE seq IN ({})",
                    marks(rows.len())
                );
                let mut values: Vec<rusqlite::types::Value> =
                    vec![owner.to_owned().into(), (now + lease_s).into()];
                values.extend(rows.iter().map(|r| rusqlite::types::Value::Integer(r.0)));
                tx.execute(&sql, params_from_iter(values)).map_err(sql_err)?;
            }
            Ok(rows)
        })?;
        self.decode(rows, true).map(Some)
    }

    fn release(&self, seqs: &[i64], owner: &str) -> Result<Option<usize>> {
        if seqs.is_empty() {
            return Ok(Some(0));
        }
        let sql = format!(
            "UPDATE xsm_outbox SET claimed_by = NULL, claimed_until = NULL WHERE claimed_by = ? AND seq IN ({})",
            marks(seqs.len())
        );
        let mut values: Vec<rusqlite::types::Value> = vec![owner.to_owned().into()];
        values.extend(seqs.iter().map(|s| rusqlite::types::Value::Integer(*s)));
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(&sql, params_from_iter(values)).map(Some).map_err(sql_err)
        })
    }
}
