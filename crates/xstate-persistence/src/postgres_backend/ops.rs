//! The statements behind [`PostgresStore`](super::PostgresStore), one async function each.
//!
//! Python reference: `contrib/sqlalchemy/_ops.py`. Optimistic writes lead with the WRITE, never
//! with a read: `INSERT .. ON CONFLICT DO NOTHING` for `expected_version = 0`, a conditional
//! `UPDATE .. WHERE key AND version` for `N`, a single upsert for `None`; the follow-up `SELECT`
//! only shapes the [`StoreError::Conflict`]. Every function runs in its own transaction.

use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Row, SqlSafeStr};
use xstate_core::snapshot::Deadline;

use super::db::FailResult;
use crate::memory_store::{check_record_fields, deadline_from_value, delay_ms_as_i64};
use crate::store::{ForgetReport, SaveOptions, StoreError, StoredSnapshot, wall_now};

/// The snapshot table: its plain name (the `source` of deadline and lock rows) and the quoted
/// form spliced into statements.
#[derive(Debug, Clone)]
pub(super) struct Table {
    pub(super) raw: String,
    pub(super) sql: String,
}

fn sql_count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn version_of(raw: i32) -> u64 {
    u64::try_from(raw).unwrap_or(0)
}

// Reads

/// Read a record and its deadlines from one consistent snapshot of the database.
pub(super) async fn load(
    pool: &PgPool,
    t: &Table,
    key: &str,
) -> FailResult<Option<StoredSnapshot>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "SELECT snapshot, version, machine_version, updated_at FROM {} WHERE key = $1",
        t.sql
    );
    let Some(row) = sqlx::query(AssertSqlSafe(sql)).bind(key).fetch_optional(&mut *tx).await?
    else {
        return Ok(None);
    };
    let fields = check_record_fields(
        &format!("PostgresStore row for {key:?}"),
        &Value::String(row.try_get("snapshot")?),
        &Value::from(row.try_get::<i32, _>("version")?),
        &Value::String(row.try_get("machine_version")?),
        &Value::from(row.try_get::<f64, _>("updated_at")?),
    )?;
    let rows = sqlx::query(
        "SELECT state_id, entry_seq, due_at_wall, delay_ms, event_type FROM xsm_deadlines WHERE \
         source = $1 AND key = $2 ORDER BY due_at_wall, id",
    )
    .bind(&t.raw)
    .bind(key)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut deadlines = Vec::with_capacity(rows.len());
    for r in &rows {
        let rec = json!({
            "state_id": r.try_get::<String, _>("state_id")?,
            "entry_seq": r.try_get::<i32, _>("entry_seq")?,
            "due_at_wall": r.try_get::<f64, _>("due_at_wall")?,
            "delay_ms": r.try_get::<i64, _>("delay_ms")?,
            "event_type": r.try_get::<String, _>("event_type")?,
        });
        deadlines.push(deadline_from_value(&rec).map_err(|why| {
            StoreError::Corrupt(format!("PostgresStore deadline row for {key:?}: {why}."))
        })?);
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

/// Keys starting with `prefix`, ascending by code point (`COLLATE "C"`: byte order of UTF-8),
/// whatever collation the database was created with.
pub(super) async fn list_keys(
    pool: &PgPool,
    t: &Table,
    prefix: &str,
    limit: usize,
) -> FailResult<Vec<String>> {
    let sql = format!(
        "SELECT key FROM {} WHERE substr(key, 1, $1) = $2 ORDER BY key COLLATE \"C\" LIMIT $3",
        t.sql
    );
    let prefix_chars = i32::try_from(prefix.chars().count()).unwrap_or(i32::MAX);
    let keys = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(prefix_chars)
        .bind(prefix)
        .bind(sql_count(limit))
        .fetch_all(pool)
        .await?;
    Ok(keys)
}

/// Keys with a deadline due at or before `until_wall`, earliest first.
pub(super) async fn due_keys(
    pool: &PgPool,
    t: &Table,
    until_wall: f64,
    limit: usize,
) -> FailResult<Vec<String>> {
    let keys = sqlx::query_scalar(
        "SELECT key FROM xsm_deadlines WHERE source = $1 AND due_at_wall <= $2 GROUP BY key \
         ORDER BY MIN(due_at_wall), key COLLATE \"C\" LIMIT $3",
    )
    .bind(&t.raw)
    .bind(until_wall)
    .bind(sql_count(limit))
    .fetch_all(pool)
    .await?;
    Ok(keys)
}

/// Number of snapshot rows (for `health`).
pub(super) async fn count_keys(pool: &PgPool, t: &Table) -> FailResult<i64> {
    let n = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {}", t.sql)))
        .fetch_one(pool)
        .await?;
    Ok(n)
}

// Writes

async fn current_version(conn: &mut PgConnection, t: &Table, key: &str) -> FailResult<Option<u64>> {
    let found: Option<i32> =
        sqlx::query_scalar(AssertSqlSafe(format!("SELECT version FROM {} WHERE key = $1", t.sql)))
            .bind(key)
            .fetch_optional(conn)
            .await?;
    Ok(found.map(version_of))
}

/// Always fails: reads the stored version for the report and returns the conflict.
async fn conflict<T>(
    conn: &mut PgConnection,
    t: &Table,
    key: &str,
    expected: u64,
) -> FailResult<T> {
    let actual = current_version(conn, t, key).await?;
    Err(StoreError::Conflict { key: key.to_owned(), expected, actual }.into())
}

/// `expected_version = 0`: create only.
async fn create_only(
    conn: &mut PgConnection,
    t: &Table,
    (key, snapshot, machine_version): (&str, &str, &str),
    now: f64,
) -> FailResult<i32> {
    let sql = format!(
        "INSERT INTO {} (key, snapshot, version, machine_version, updated_at) VALUES ($1, $2, 1, \
         $3, $4) ON CONFLICT (key) DO NOTHING",
        t.sql
    );
    let done = sqlx::query(AssertSqlSafe(sql))
        .bind(key)
        .bind(snapshot)
        .bind(machine_version)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    if done.rows_affected() == 1 { Ok(1) } else { conflict(conn, t, key, 0).await }
}

/// `expected_version = N`: update only the row still at version `N`.
async fn update_at(
    conn: &mut PgConnection,
    t: &Table,
    (key, snapshot, machine_version): (&str, &str, &str),
    expected: u64,
    now: f64,
) -> FailResult<i32> {
    // A version beyond the INTEGER column can never match a stored row.
    let Some(from) = i32::try_from(expected).ok().filter(|v| *v < i32::MAX) else {
        return conflict(conn, t, key, expected).await;
    };
    let sql = format!(
        "UPDATE {} SET snapshot = $1, version = $2, machine_version = $3, updated_at = $4 WHERE \
         key = $5 AND version = $6",
        t.sql
    );
    let done = sqlx::query(AssertSqlSafe(sql))
        .bind(snapshot)
        .bind(from + 1)
        .bind(machine_version)
        .bind(now)
        .bind(key)
        .bind(from)
        .execute(&mut *conn)
        .await?;
    if done.rows_affected() == 1 { Ok(from + 1) } else { conflict(conn, t, key, expected).await }
}

/// `expected_version = None`: create or bump, atomically (no window between UPDATE and INSERT).
async fn upsert(
    conn: &mut PgConnection,
    t: &Table,
    (key, snapshot, machine_version): (&str, &str, &str),
    now: f64,
) -> FailResult<i32> {
    let sql = format!(
        "INSERT INTO {t} AS s (key, snapshot, version, machine_version, updated_at) VALUES ($1, \
         $2, 1, $3, $4) ON CONFLICT (key) DO UPDATE SET snapshot = EXCLUDED.snapshot, version = \
         s.version + 1, machine_version = EXCLUDED.machine_version, updated_at = \
         EXCLUDED.updated_at RETURNING version",
        t = t.sql
    );
    let version = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(key)
        .bind(snapshot)
        .bind(machine_version)
        .bind(now)
        .fetch_one(conn)
        .await?;
    Ok(version)
}

/// Replace the deadline rows of (`source`, `key`); returns how many were removed.
async fn replace_deadlines(
    conn: &mut PgConnection,
    source: &str,
    key: &str,
    deadlines: &[Deadline],
) -> FailResult<u64> {
    let removed = sqlx::query("DELETE FROM xsm_deadlines WHERE source = $1 AND key = $2")
        .bind(source)
        .bind(key)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    for d in deadlines {
        let entry_seq = i32::try_from(d.entry_seq).map_err(|_| {
            StoreError::Other("invalid deadline: 'entry_seq' exceeds the INTEGER column".into())
        })?;
        let delay_ms = delay_ms_as_i64(&d.delay_ms).ok_or_else(|| {
            StoreError::Other("invalid deadline: 'delay_ms' is not an integer".into())
        })?;
        sqlx::query(
            "INSERT INTO xsm_deadlines (source, key, state_id, entry_seq, due_at_wall, delay_ms, \
             event_type) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(source)
        .bind(key)
        .bind(&d.state_id)
        .bind(entry_seq)
        .bind(d.due_at_wall)
        .bind(delay_ms)
        .bind(&d.event_type)
        .execute(&mut *conn)
        .await?;
    }
    Ok(removed)
}

/// Write a snapshot and its deadlines in one transaction; returns the new version.
pub(super) async fn save(
    pool: &PgPool,
    t: &Table,
    key: &str,
    snapshot_json: &str,
    options: &SaveOptions<'_>,
    deadlines: &[Deadline],
) -> FailResult<u64> {
    let mut tx = pool.begin().await?;
    let now = wall_now();
    let fields = (key, snapshot_json, options.machine_version);
    let version = match options.expected_version {
        Some(0) => create_only(&mut tx, t, fields, now).await?,
        Some(expected) => update_at(&mut tx, t, fields, expected, now).await?,
        None => upsert(&mut tx, t, fields, now).await?,
    };
    replace_deadlines(&mut tx, &t.raw, key, deadlines).await?;
    tx.commit().await?;
    Ok(version_of(version))
}

/// Delete a snapshot and its deadlines.
pub(super) async fn delete(pool: &PgPool, t: &Table, key: &str) -> FailResult<bool> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query(AssertSqlSafe(format!("DELETE FROM {} WHERE key = $1", t.sql)))
        .bind(key)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    replace_deadlines(&mut tx, &t.raw, key, &[]).await?;
    tx.commit().await?;
    Ok(removed > 0)
}

async fn delete_rows(
    conn: &mut PgConnection,
    sql: impl SqlSafeStr,
    binds: &[&str],
) -> FailResult<u64> {
    let mut query = sqlx::query(sql);
    for b in binds {
        query = query.bind(*b);
    }
    Ok(query.execute(conn).await?.rows_affected())
}

/// X0.5: the record, its deadlines, its lock row AND its transition log, in one transaction.
pub(super) async fn forget(pool: &PgPool, t: &Table, key: &str) -> FailResult<ForgetReport> {
    let mut tx = pool.begin().await?;
    let deadlines = replace_deadlines(&mut tx, &t.raw, key, &[]).await?;
    let locks = delete_rows(
        &mut tx,
        "DELETE FROM xsm_locks WHERE source = $1 AND key = $2",
        &[&t.raw, key],
    )
    .await?;
    let log_entries =
        delete_rows(&mut tx, "DELETE FROM xsm_transitions WHERE machine_id = $1", &[key]).await?;
    let snapshots = delete_rows(
        &mut tx,
        AssertSqlSafe(format!("DELETE FROM {} WHERE key = $1", t.sql)),
        &[key],
    )
    .await?;
    tx.commit().await?;
    Ok(ForgetReport {
        snapshots,
        deadlines,
        other: vec![("locks".to_owned(), locks), ("log_entries".to_owned(), log_entries)],
    })
}

// Lease rows

/// One attempt at the lease row: take it when absent or expired, in a single statement.
pub(super) async fn try_lock(
    pool: &PgPool,
    t: &Table,
    key: &str,
    owner: &str,
    ttl_s: f64,
) -> FailResult<bool> {
    let now = wall_now();
    let done = sqlx::query(
        "INSERT INTO xsm_locks AS l (source, key, owner, expires_at) VALUES ($1, $2, $3, $4) ON \
         CONFLICT (source, key) DO UPDATE SET owner = EXCLUDED.owner, expires_at = \
         EXCLUDED.expires_at WHERE l.expires_at < $5",
    )
    .bind(&t.raw)
    .bind(key)
    .bind(owner)
    .bind(now + ttl_s)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Release the lease row, only while `owner` still holds it.
pub(super) async fn unlock(pool: &PgPool, source: &str, key: &str, owner: &str) -> FailResult<()> {
    sqlx::query("DELETE FROM xsm_locks WHERE source = $1 AND key = $2 AND owner = $3")
        .bind(source)
        .bind(key)
        .bind(owner)
        .execute(pool)
        .await?;
    Ok(())
}
