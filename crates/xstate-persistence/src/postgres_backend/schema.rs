//! Tables, the schema-version row and the DDL of the Postgres backends.
//!
//! Python reference: `contrib/sqlalchemy/_schema.py`. The statements below are what `SQLAlchemy`'s
//! `create_all` emits on the PostgreSQL dialect for `build_tables`, so a database created by
//! either runtime is used by the other. Only the snapshot table's name is configurable; the
//! auxiliary tables are shared (and carry a `source` column where two owners could collide).

use sqlx::{AssertSqlSafe, PgConnection, PgPool};

use super::db::{ADVISORY_CLASS, FailResult};
use crate::store::StoreError;

/// Layout version recorded in `xsm_schema` (Python `SCHEMA_VERSION`).
pub const SCHEMA_VERSION: i32 = 1;
/// The `component` of this layout's row in `xsm_schema` (Python `_COMPONENT`).
const COMPONENT: &str = "sqlalchemy";

/// Default snapshot table (Python `DEFAULT_TABLE`).
pub(super) const DEFAULT_TABLE: &str = "xsm_snapshots";
pub(super) const SCHEMA_TABLE: &str = "xsm_schema";
pub(super) const DEADLINES: &str = "xsm_deadlines";
pub(super) const LOCKS: &str = "xsm_locks";
pub(super) const INBOX: &str = "xsm_inbox";
pub(super) const TRANSITIONS: &str = "xsm_transitions";

/// One table to create: its name and the statements (table first, then its indexes).
struct TableDdl {
    name: String,
    statements: Vec<String>,
}

fn table(name: &str, statements: &[&str]) -> TableDdl {
    TableDdl {
        name: name.to_owned(),
        statements: statements.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// The six tables in Python's `XsmTables.all()` order. `snapshots` is the quoted, validated name.
fn tables(snapshots_raw: &str, snapshots_sql: &str) -> Vec<TableDdl> {
    vec![
        table(
            SCHEMA_TABLE,
            &["CREATE TABLE IF NOT EXISTS xsm_schema (component VARCHAR(64) NOT NULL, version \
               INTEGER NOT NULL, PRIMARY KEY (component))"],
        ),
        TableDdl {
            name: snapshots_raw.to_owned(),
            statements: vec![format!(
                "CREATE TABLE IF NOT EXISTS {snapshots_sql} (key VARCHAR(200) NOT NULL, snapshot \
                 TEXT NOT NULL, version INTEGER NOT NULL, machine_version VARCHAR(255) NOT NULL, \
                 updated_at FLOAT NOT NULL, PRIMARY KEY (key))"
            )],
        },
        table(
            DEADLINES,
            &[
                "CREATE TABLE IF NOT EXISTS xsm_deadlines (id SERIAL NOT NULL, source \
                 VARCHAR(255) NOT NULL, key VARCHAR(200) NOT NULL, state_id VARCHAR(512) NOT \
                 NULL, entry_seq INTEGER NOT NULL, due_at_wall FLOAT NOT NULL, delay_ms BIGINT \
                 NOT NULL, event_type VARCHAR(512) NOT NULL, PRIMARY KEY (id))",
                "CREATE INDEX IF NOT EXISTS xsm_deadlines_due ON xsm_deadlines (due_at_wall)",
                "CREATE INDEX IF NOT EXISTS xsm_deadlines_key ON xsm_deadlines (source, key)",
            ],
        ),
        table(
            LOCKS,
            &["CREATE TABLE IF NOT EXISTS xsm_locks (source VARCHAR(255) NOT NULL, key \
               VARCHAR(200) NOT NULL, owner VARCHAR(64) NOT NULL, expires_at FLOAT NOT NULL, \
               PRIMARY KEY (source, key))"],
        ),
        table(
            INBOX,
            &[
                "CREATE TABLE IF NOT EXISTS xsm_inbox (scope VARCHAR(400) NOT NULL, key \
                 VARCHAR(255) NOT NULL, fingerprint VARCHAR(128) NOT NULL, receipt TEXT, \
                 expires_at FLOAT, PRIMARY KEY (scope, key))",
                "CREATE INDEX IF NOT EXISTS xsm_inbox_exp ON xsm_inbox (expires_at)",
            ],
        ),
        table(
            TRANSITIONS,
            &[
                "CREATE TABLE IF NOT EXISTS xsm_transitions (machine_id VARCHAR(255) NOT NULL, \
                 seq INTEGER NOT NULL, ts FLOAT NOT NULL, record TEXT NOT NULL, PRIMARY KEY \
                 (machine_id, seq))",
                "CREATE INDEX IF NOT EXISTS xsm_transitions_ts ON xsm_transitions (ts)",
            ],
        ),
    ]
}

fn newer(current: i32) -> StoreError {
    StoreError::Corrupt(format!(
        "xstate-statemachine [sqlalchemy] schema is version {current}, newer than this library \
         supports ({SCHEMA_VERSION}). Upgrade xstate-persistence."
    ))
}

/// Is `name` a table visible on the connection's `search_path`? (`SQLAlchemy`'s `has_table`.)
async fn has_table(conn: &mut PgConnection, name: &str) -> FailResult<bool> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_class c WHERE c.relname = $1 AND c.relkind \
         IN ('r', 'p') AND pg_catalog.pg_table_is_visible(c.oid))",
    )
    .bind(name)
    .fetch_one(conn)
    .await?;
    Ok(found)
}

async fn stored_version(conn: &mut PgConnection) -> FailResult<Option<i32>> {
    let row = sqlx::query_scalar("SELECT version FROM xsm_schema WHERE component = $1")
        .bind(COMPONENT)
        .fetch_optional(conn)
        .await?;
    Ok(row)
}

/// Verify an existing schema WITHOUT creating anything (`create_tables = false`): a clear error
/// when tables are missing (run your migrations) or were written by a newer release.
pub(super) async fn check_schema(
    pool: &PgPool,
    snapshots_raw: &str,
    snapshots_sql: &str,
) -> FailResult<()> {
    let mut conn = pool.acquire().await?;
    let mut missing = Vec::new();
    for ddl in tables(snapshots_raw, snapshots_sql) {
        if !has_table(&mut conn, &ddl.name).await? {
            missing.push(ddl.name);
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(StoreError::Other(format!(
            "xstate-statemachine [sqlalchemy] tables are missing: {}. Run your migrations (or \
             let the store create them: PostgresStoreOptions::create_tables(true)).",
            missing.join(", ")
        ))
        .into());
    }
    match stored_version(&mut conn).await? {
        Some(current) if current > SCHEMA_VERSION => Err(newer(current).into()),
        _ => Ok(()),
    }
}

/// Create the missing tables and record the schema version; idempotent. Refuses a database
/// written by a newer release.
///
/// Runs in one transaction under an advisory lock, so several Rust processes opening a fresh
/// database at once cannot trip over each other's `CREATE TABLE`.
pub(super) async fn ensure_schema(
    pool: &PgPool,
    snapshots_raw: &str,
    snapshots_sql: &str,
) -> FailResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, 0)")
        .bind(ADVISORY_CLASS)
        .execute(&mut *tx)
        .await?;
    for ddl in tables(snapshots_raw, snapshots_sql) {
        if has_table(&mut tx, &ddl.name).await? {
            continue;
        }
        for statement in &ddl.statements {
            sqlx::query(AssertSqlSafe(statement.as_str())).execute(&mut *tx).await?;
        }
    }
    sqlx::query(
        "INSERT INTO xsm_schema (component, version) VALUES ($1, $2) ON CONFLICT (component) DO \
         NOTHING",
    )
    .bind(COMPONENT)
    .bind(SCHEMA_VERSION)
    .execute(&mut *tx)
    .await?;
    let current = stored_version(&mut tx).await?.unwrap_or(SCHEMA_VERSION);
    if current > SCHEMA_VERSION {
        return Err(newer(current).into());
    }
    if current < SCHEMA_VERSION {
        return Err(StoreError::Corrupt(format!(
            "xstate-statemachine [sqlalchemy] schema is version {current}; no upgrade path to \
             {SCHEMA_VERSION}"
        ))
        .into());
    }
    tx.commit().await?;
    Ok(())
}
