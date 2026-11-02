//! [`SqliteDeadLetterStore`]: dead letters in the Python `xsm_dead_letters` / `xsm_dlq_audit`
//! tables, so a file can be shared with the Python library and its `xsm dlq` CLI.

use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use xstate_core::redact::RedactKeys;

use super::{DeadLetter, DeadLetterSink, DeadLetterStore, check_cutoff, redact_record};
use crate::error::{EdaError, Result, sql_err};
use crate::sqlite_util;

const CREATE: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS xsm_dead_letters (
        id          TEXT PRIMARY KEY,
        taken_at    REAL NOT NULL,
        reason      TEXT NOT NULL,
        machine_id  TEXT NOT NULL,
        topic       TEXT,
        resolved_at REAL,
        record      TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS xsm_dead_letters_taken ON xsm_dead_letters(taken_at)",
    "CREATE TABLE IF NOT EXISTS xsm_dlq_audit (
        seq       INTEGER PRIMARY KEY AUTOINCREMENT,
        at        REAL NOT NULL,
        action    TEXT NOT NULL,
        record_id TEXT,
        actor     TEXT NOT NULL,
        reason    TEXT NOT NULL,
        detail    TEXT NOT NULL
    )",
];

/// One row of the audit trail.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    /// Epoch seconds.
    pub at: f64,
    /// `replay`, `purge`, ...
    pub action: String,
    /// The record concerned, if any.
    pub record_id: Option<String>,
    /// Who did it.
    pub actor: String,
    /// Why.
    pub reason: String,
    /// Redacted details.
    pub detail: Value,
}

/// Dead letters in SQLite: the default store behind an operator CLI.
#[derive(Debug)]
pub struct SqliteDeadLetterStore {
    conn: Mutex<Connection>,
}

impl SqliteDeadLetterStore {
    /// Open (creating if needed) a database file, with the `SQLiteStore` schema Python creates
    /// alongside.
    ///
    /// # Errors
    /// I/O or SQLite failure; a `SQLiteStore` schema newer than this library supports.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::init(sqlite_util::open(path.as_ref())?)
    }

    /// A private in-memory store.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(sqlite_util::open_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        sqlite_util::immediate(&mut conn, |tx| {
            for stmt in CREATE {
                tx.execute_batch(stmt).map_err(sql_err)?;
            }
            Ok(())
        })?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The audit trail in sequence order.
    ///
    /// # Errors
    /// SQLite failure or an undecodable `detail`.
    pub fn audit_log(&self) -> Result<Vec<AuditEntry>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT at, action, record_id, actor, reason, detail FROM xsm_dlq_audit ORDER BY seq")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, String>(5)?))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (at, action, record_id, actor, reason, detail) = row.map_err(sql_err)?;
            let detail = serde_json::from_str(&detail)
                .map_err(|e| EdaError::InvalidArgument(format!("corrupt audit detail: {e}")))?;
            out.push(AuditEntry { at, action, record_id, actor, reason, detail });
        }
        Ok(out)
    }

    fn decode(record: &str, resolved_at: Option<f64>) -> Result<DeadLetter> {
        let raw: Value = serde_json::from_str(record)
            .map_err(|e| EdaError::InvalidArgument(format!("corrupt dead letter: {e}")))?;
        let mut rec = DeadLetter::from_dict(&raw)?;
        rec.resolved_at = resolved_at;
        Ok(rec)
    }
}

impl DeadLetterSink for SqliteDeadLetterStore {
    /// `INSERT OR REPLACE` the redacted record.
    fn put(&self, record: DeadLetter) -> Result<()> {
        let rec = redact_record(&record, &RedactKeys::default());
        let json = rec.to_json();
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO xsm_dead_letters\
                 (id, taken_at, reason, machine_id, topic, resolved_at, record) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.id,
                    rec.taken_at,
                    rec.reason,
                    rec.machine_id,
                    rec.topic,
                    rec.resolved_at,
                    json
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
    }
}

impl DeadLetterStore for SqliteDeadLetterStore {
    fn get(&self, record_id: &str) -> Result<Option<DeadLetter>> {
        let row: Option<(String, Option<f64>)> = self
            .conn()
            .query_row(
                "SELECT record, resolved_at FROM xsm_dead_letters WHERE id = ?1",
                [record_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        row.map(|(rec, at)| Self::decode(&rec, at)).transpose()
    }

    fn list(&self, include_resolved: bool, limit: usize) -> Result<Vec<DeadLetter>> {
        let mut sql = String::from("SELECT record, resolved_at FROM xsm_dead_letters");
        if !include_resolved {
            sql.push_str(" WHERE resolved_at IS NULL");
        }
        sql.push_str(" ORDER BY taken_at, id LIMIT ?1");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?)))
            .map_err(sql_err)?;
        rows.map(|row| row.map_err(sql_err).and_then(|(rec, at)| Self::decode(&rec, at))).collect()
    }

    fn mark_resolved(&self, record_id: &str, when: f64) -> Result<bool> {
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(
                "UPDATE xsm_dead_letters SET resolved_at = ?1 WHERE id = ?2",
                params![when, record_id],
            )
            .map(|n| n > 0)
            .map_err(sql_err)
        })
    }

    fn delete(&self, record_id: &str) -> Result<bool> {
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute("DELETE FROM xsm_dead_letters WHERE id = ?1", [record_id])
                .map(|n| n > 0)
                .map_err(sql_err)
        })
    }

    fn purge_older_than(&self, cutoff_wall: f64) -> Result<usize> {
        check_cutoff(cutoff_wall)?;
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute("DELETE FROM xsm_dead_letters WHERE taken_at < ?1", [cutoff_wall])
                .map_err(sql_err)
        })
    }

    fn count(&self) -> Result<usize> {
        let n: i64 = self
            .conn()
            .query_row("SELECT count(*) FROM xsm_dead_letters", [], |r| r.get(0))
            .map_err(sql_err)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn audit(
        &self,
        action: &str,
        record_id: Option<&str>,
        reason: &str,
        detail: &Value,
        actor: Option<&str>,
    ) -> Result<()> {
        let detail = xstate_core::pyjson::dumps(&RedactKeys::default().redact(detail));
        let actor = actor.map_or_else(whoami, str::to_owned);
        sqlite_util::immediate(&mut self.conn(), |tx| {
            tx.execute(
                "INSERT INTO xsm_dlq_audit(at, action, record_id, actor, reason, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![crate::py::wall_now(), action, record_id, actor, reason, detail],
            )
            .map(|_| ())
            .map_err(sql_err)
        })
    }
}

/// The login name (Python `getpass.getuser()`), `"unknown"` when there is none.
fn whoami() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn put_redacts_and_round_trips() {
        let store = SqliteDeadLetterStore::open_in_memory().unwrap();
        let mut rec =
            DeadLetter::new("m", "", json!({"type": "E", "payload": {"password": "p"}}), 5.0);
        rec.id = "r1".into();
        store.put(rec).unwrap();
        let got = store.get("r1").unwrap().unwrap();
        assert_eq!(got.event["payload"]["password"], "***");
        assert_eq!(store.count().unwrap(), 1);
        assert!(store.mark_resolved("r1", 6.0).unwrap());
        assert_eq!(store.get("r1").unwrap().unwrap().resolved_at, Some(6.0));
        assert_eq!(store.list(false, 10).unwrap().len(), 0);
        assert_eq!(store.list(true, 10).unwrap().len(), 1);
        store.audit("replay", Some("r1"), "why", &json!({"token": "t"}), Some("ops")).unwrap();
        let log = store.audit_log().unwrap();
        assert_eq!(log[0].actor, "ops");
        assert_eq!(log[0].detail, json!({"token": "***"}));
        assert_eq!(store.purge_older_than(10.0).unwrap(), 1);
        assert!(!store.delete("r1").unwrap());
    }
}
