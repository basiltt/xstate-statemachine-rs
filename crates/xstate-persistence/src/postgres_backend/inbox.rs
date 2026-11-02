//! [`PostgresInbox`]: the idempotency inbox in the `xsm_inbox` table, as Python's
//! `SQLAlchemyInbox` writes it.
//!
//! Python reference: `contrib/sqlalchemy/inbox_log.py`. Every expiry is computed on the caller's
//! wall clock (as in Python); a claim is one `INSERT .. ON CONFLICT` statement that also reclaims
//! an expired row, so two processes racing for a key cannot both win.

use std::sync::Arc;

use sqlx::{PgPool, Row};

use super::db::{Db, FailResult, reject_nul};
use super::store::PostgresStore;
use crate::idempotency::{InboxEntry, InboxStore};
use crate::store::{Result, StoreError, wall_now};

/// `expires_at` for a claim or mark made at `now` with `ttl_s` to live; `None` = never.
fn expiry(ttl_s: Option<f64>, now: f64) -> Result<Option<f64>> {
    match ttl_s {
        None => Ok(None),
        Some(ttl) if ttl.is_finite() => Ok(Some(now + ttl)),
        Some(ttl) => Err(StoreError::Other(format!("ttl_s must be a finite number, got {ttl}"))),
    }
}

/// [`InboxStore`] in the `xsm_inbox` table of the database a [`PostgresStore`] works on.
///
/// Built from the store ([`PostgresInbox::new`]) and sharing its pool and runtime, so the same
/// rules apply: its methods block the calling thread and must not be called from a task running on
/// that runtime (use `spawn_blocking`). Each call is its own short transaction; unlike Python's
/// `SQLAlchemyInbox` inside `lock()`, a mark does not commit together with the snapshot save, so
/// the in-snapshot processed ring covers the save-then-mark gap, as for the other Rust inboxes.
///
/// `PostgresStore::forget` does not touch inbox rows (they belong to a principal scope, not an
/// instance); call [`InboxStore::forget`] with the scope.
#[derive(Debug)]
pub struct PostgresInbox {
    db: Arc<Db>,
}

impl PostgresInbox {
    /// An inbox over the database of `store`.
    #[must_use]
    pub fn new(store: &PostgresStore) -> Self {
        Self { db: Arc::clone(store.db()) }
    }
}

fn check_names(scope: &str, key: &str) -> Result<()> {
    reject_nul("inbox scope", scope)?;
    reject_nul("inbox key", key)
}

async fn get_in(pool: &PgPool, scope: &str, key: &str, now: f64) -> FailResult<Option<InboxEntry>> {
    let row = sqlx::query(
        "SELECT fingerprint, receipt, expires_at FROM xsm_inbox WHERE scope = $1 AND key = $2 AND \
         (expires_at IS NULL OR expires_at > $3)",
    )
    .bind(scope)
    .bind(key)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        Ok(InboxEntry {
            fingerprint: r.try_get("fingerprint")?,
            receipt_json: r.try_get("receipt")?,
            expires_at: r.try_get("expires_at")?,
        })
    })
    .transpose()
}

async fn claim_in(
    pool: &PgPool,
    (scope, key, fingerprint): (&str, &str, &str),
    expires_at: Option<f64>,
    now: f64,
) -> FailResult<bool> {
    // One statement: insert, or take over a row whose expiry has passed. A live row wins.
    let done = sqlx::query(
        "INSERT INTO xsm_inbox AS i (scope, key, fingerprint, receipt, expires_at) VALUES ($1, $2, \
         $3, NULL, $4) ON CONFLICT (scope, key) DO UPDATE SET fingerprint = EXCLUDED.fingerprint, \
         receipt = NULL, expires_at = EXCLUDED.expires_at WHERE i.expires_at IS NOT NULL AND \
         i.expires_at <= $5",
    )
    .bind(scope)
    .bind(key)
    .bind(fingerprint)
    .bind(expires_at)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

async fn execute(pool: &PgPool, sql: &'static str, binds: &[&str]) -> FailResult<u64> {
    let mut query = sqlx::query(sql);
    for text in binds {
        query = query.bind(*text);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

impl InboxStore for PostgresInbox {
    fn get(&self, scope: &str, key: &str) -> Result<Option<InboxEntry>> {
        check_names(scope, key)?;
        self.db.run(key, get_in(self.db.pool(), scope, key, wall_now()))
    }

    fn claim(&self, scope: &str, key: &str, fingerprint: &str, ttl_s: Option<f64>) -> Result<bool> {
        check_names(scope, key)?;
        reject_nul("fingerprint", fingerprint)?;
        let now = wall_now();
        let expires_at = expiry(ttl_s, now)?;
        self.db.run(key, claim_in(self.db.pool(), (scope, key, fingerprint), expires_at, now))
    }

    fn mark(&self, scope: &str, key: &str, receipt_json: &str, ttl_s: Option<f64>) -> Result<()> {
        check_names(scope, key)?;
        reject_nul("receipt", receipt_json)?;
        let expires_at = expiry(ttl_s, wall_now())?;
        self.db.run(key, async {
            sqlx::query(
                "UPDATE xsm_inbox SET receipt = $1, expires_at = $2 WHERE scope = $3 AND key = $4",
            )
            .bind(receipt_json)
            .bind(expires_at)
            .bind(scope)
            .bind(key)
            .execute(self.db.pool())
            .await?;
            Ok(())
        })
    }

    fn release(&self, scope: &str, key: &str) -> Result<()> {
        check_names(scope, key)?;
        let sql = "DELETE FROM xsm_inbox WHERE scope = $1 AND key = $2 AND receipt IS NULL";
        self.db.run(key, execute(self.db.pool(), sql, &[scope, key])).map(|_| ())
    }

    fn purge_expired(&self, now: Option<f64>) -> Result<u64> {
        let at = match now {
            None => wall_now(),
            Some(t) if t.is_finite() => t,
            Some(t) => {
                return Err(StoreError::Other(format!("now must be a finite number, got {t}")));
            }
        };
        self.db.run("<db>", async {
            let done = sqlx::query(
                "DELETE FROM xsm_inbox WHERE expires_at IS NOT NULL AND expires_at <= $1",
            )
            .bind(at)
            .execute(self.db.pool())
            .await?;
            Ok(done.rows_affected())
        })
    }

    fn forget(&self, scope: &str) -> Result<u64> {
        reject_nul("inbox scope", scope)?;
        let sql = "DELETE FROM xsm_inbox WHERE scope = $1";
        self.db.run(scope, execute(self.db.pool(), sql, &[scope]))
    }
}
