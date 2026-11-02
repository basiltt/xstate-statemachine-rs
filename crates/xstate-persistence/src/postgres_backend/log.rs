//! [`PostgresLog`]: the transition log in the `xsm_transitions` table, as Python's
//! `SQLAlchemyLog` writes it.
//!
//! Python reference: `contrib/sqlalchemy/inbox_log.py`. The primary key `(machine_id, seq)` is what
//! stops two writers from storing the same `seq`; `append_next` additionally serialises writers of
//! one key with a transaction-scoped advisory lock, and retries when a writer that does not take
//! it (Python's `next_seq` + `append`) wins the race.

use std::sync::Arc;

use sqlx::{PgConnection, PgPool, Row};

use super::db::{ADVISORY_CLASS, Db, Fail, FailResult, is_unique_violation, reject_nul};
use super::store::PostgresStore;
use crate::log::{TransitionLogStore, TransitionRecord};
use crate::store::{Result, StoreError};

/// How often `append_next` re-reads the top `seq` after losing a race before giving up loudly.
const MAX_APPEND_RETRIES: usize = 100;

/// Reject a record no store may persist: `seq < 1` or a non-finite `ts`.
fn check_append(record: &TransitionRecord) -> Result<()> {
    if record.seq < 1 {
        return Err(StoreError::Other(format!("seq must be an int >= 1, got {}", record.seq)));
    }
    if !record.ts.is_finite() {
        return Err(StoreError::Other(format!("ts must be a finite number, got {}", record.ts)));
    }
    reject_nul("machine_id", &record.machine_id)
}

/// A purge cutoff that would silently erase everything (`NaN` makes every comparison false).
fn check_cutoff(cutoff_ts: f64) -> Result<f64> {
    if cutoff_ts.is_nan() || cutoff_ts == f64::NEG_INFINITY {
        return Err(StoreError::Other(format!("cutoff_ts must not be NaN / -inf: {cutoff_ts}")));
    }
    Ok(cutoff_ts)
}

/// `seq` as the INTEGER column holds it.
fn column_seq(seq: u64) -> Result<i32> {
    i32::try_from(seq)
        .map_err(|_| StoreError::Other(format!("seq {seq} exceeds the INTEGER column (2**31-1)")))
}

fn from_column_seq(seq: i32) -> u64 {
    u64::try_from(seq).unwrap_or(0)
}

fn already_exists(record: &TransitionRecord) -> StoreError {
    StoreError::Other(format!(
        "transition ({}, seq {}) already exists",
        record.machine_id, record.seq
    ))
}

/// [`TransitionLogStore`] in the `xsm_transitions` table of the database a [`PostgresStore`] works
/// on. `PostgresStore::forget(key)` erases a key's rows (the log is keyed by the store key).
///
/// Built from the store ([`PostgresLog::new`]) and sharing its pool and runtime: its methods block
/// the calling thread and must not be called from a task running on that runtime (use
/// `spawn_blocking`). Each call is its own transaction; a record is not committed together with a
/// snapshot save, so after a crash the log can trail the snapshot.
#[derive(Debug)]
pub struct PostgresLog {
    db: Arc<Db>,
}

impl PostgresLog {
    /// A log over the database of `store`.
    #[must_use]
    pub fn new(store: &PostgresStore) -> Self {
        Self { db: Arc::clone(store.db()) }
    }
}

async fn insert(conn: &mut PgConnection, record: &TransitionRecord) -> FailResult<()> {
    let seq = column_seq(record.seq)?;
    sqlx::query(
        "INSERT INTO xsm_transitions (machine_id, seq, ts, record) VALUES ($1, $2, $3, $4)",
    )
    .bind(&record.machine_id)
    .bind(seq)
    .bind(record.ts)
    .bind(record.to_json())
    .execute(conn)
    .await?;
    Ok(())
}

/// Name a duplicate `(machine_id, seq)` as such; leave every other failure alone.
fn explain_duplicate(record: &TransitionRecord, fail: Fail) -> Fail {
    match fail {
        Fail::Db(e) if is_unique_violation(&e) => already_exists(record).into(),
        other => other,
    }
}

async fn max_seq(conn: &mut PgConnection, machine_id: &str) -> FailResult<u64> {
    let max: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0) FROM xsm_transitions WHERE machine_id = $1",
    )
    .bind(machine_id)
    .fetch_one(conn)
    .await?;
    Ok(from_column_seq(max))
}

/// One attempt at "next seq, then insert" under the per-key advisory lock. A unique violation
/// (a writer outside the lock won the `seq`) comes back as the sqlx error for the caller to retry.
async fn append_next_once(
    pool: &PgPool,
    record: &TransitionRecord,
) -> std::result::Result<TransitionRecord, Fail> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ADVISORY_CLASS)
        .bind(&record.machine_id)
        .execute(&mut *tx)
        .await?;
    let stored =
        TransitionRecord { seq: max_seq(&mut tx, &record.machine_id).await? + 1, ..record.clone() };
    check_append(&stored)?;
    insert(&mut tx, &stored).await?;
    tx.commit().await?;
    Ok(stored)
}

async fn append_next_retrying(
    pool: &PgPool,
    record: &TransitionRecord,
) -> FailResult<TransitionRecord> {
    for _ in 0..MAX_APPEND_RETRIES {
        match append_next_once(pool, record).await {
            Err(Fail::Db(e)) if is_unique_violation(&e) => {}
            other => return other,
        }
    }
    Err(StoreError::Other(format!(
        "could not allocate a seq for '{}': {MAX_APPEND_RETRIES} concurrent writers won the race",
        record.machine_id
    ))
    .into())
}

/// Rows `(seq, record JSON)` after `after_seq`, ascending, plus the corruption check Python's
/// `seq > 0` filter would hide: a row with `seq < 1` is a hole in a replay.
async fn read_rows(
    pool: &PgPool,
    machine_id: &str,
    after_seq: i32,
    limit: usize,
) -> FailResult<Vec<(i32, String)>> {
    let rows = sqlx::query(
        "SELECT seq, record FROM xsm_transitions WHERE machine_id = $1 AND seq > $2 ORDER BY seq \
         LIMIT $3",
    )
    .bind(machine_id)
    .bind(after_seq)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;
    rows.iter().map(|r| Ok((r.try_get("seq")?, r.try_get("record")?))).collect()
}

async fn has_hole(pool: &PgPool, machine_id: &str) -> FailResult<bool> {
    let hole: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM xsm_transitions WHERE machine_id = $1 AND seq < 1 LIMIT 1",
    )
    .bind(machine_id)
    .fetch_optional(pool)
    .await?;
    Ok(hole.is_some())
}

fn decode_row(machine_id: &str, seq: i32, raw: &str) -> Result<TransitionRecord> {
    let place = format!("transitions({machine_id:?}, seq={seq})");
    let record = TransitionRecord::from_json(raw)
        .map_err(|e| StoreError::Corrupt(format!("{place}: {e}")))?;
    if i64::from(seq) != i64::try_from(record.seq).unwrap_or(-1) || record.machine_id != machine_id
    {
        return Err(StoreError::Corrupt(format!(
            "{place}: row key disagrees with its record ({:?}, {})",
            record.machine_id, record.seq
        )));
    }
    Ok(record)
}

impl TransitionLogStore for PostgresLog {
    fn append(&self, record: &TransitionRecord) -> Result<()> {
        check_append(record)?;
        self.db.run(&record.machine_id, async {
            let mut tx = self.db.pool().begin().await?;
            insert(&mut tx, record).await.map_err(|f| explain_duplicate(record, f))?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        reject_nul("machine_id", &record.machine_id)?;
        self.db.run(&record.machine_id, append_next_retrying(self.db.pool(), &record))
    }

    fn next_seq(&self, machine_id: &str) -> Result<u64> {
        reject_nul("machine_id", machine_id)?;
        self.db.run(machine_id, async {
            let mut conn = self.db.pool().acquire().await?;
            Ok(max_seq(&mut conn, machine_id).await? + 1)
        })
    }

    fn read(
        &self,
        machine_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TransitionRecord>> {
        reject_nul("machine_id", machine_id)?;
        // No row can be past the INTEGER column's top.
        let Ok(after) = i32::try_from(after_seq) else { return Ok(Vec::new()) };
        let rows = self.db.run(machine_id, async {
            if after_seq == 0 && has_hole(self.db.pool(), machine_id).await? {
                return Err(StoreError::Corrupt(format!(
                    "transitions({machine_id:?}): row with seq < 1"
                ))
                .into());
            }
            read_rows(self.db.pool(), machine_id, after, limit).await
        })?;
        rows.iter().map(|(seq, raw)| decode_row(machine_id, *seq, raw)).collect()
    }

    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64> {
        let cutoff = check_cutoff(cutoff_ts)?;
        self.db.run("<db>", async {
            let done = sqlx::query("DELETE FROM xsm_transitions WHERE ts < $1")
                .bind(cutoff)
                .execute(self.db.pool())
                .await?;
            Ok(done.rows_affected())
        })
    }

    fn forget(&self, machine_id: &str) -> Result<u64> {
        reject_nul("machine_id", machine_id)?;
        self.db.run(machine_id, async {
            let done = sqlx::query("DELETE FROM xsm_transitions WHERE machine_id = $1")
                .bind(machine_id)
                .execute(self.db.pool())
                .await?;
            Ok(done.rows_affected())
        })
    }
}
