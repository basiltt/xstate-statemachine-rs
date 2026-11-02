//! Runtime bridge, error mapping and identifier rules shared by the Postgres backends.
//!
//! sqlx is async and the store traits are synchronous, so every operation is one future driven to
//! completion with [`Handle::block_on`]. `connect` owns a small multi-thread runtime for it;
//! `from_pool` borrows the handle of a runtime the caller already runs.

use std::borrow::Cow;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::error::DatabaseError;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::runtime::{Builder, Handle, Runtime};

use crate::store::{Result, StoreError};

/// Worker threads of the runtime a `connect`ed store owns: enough that a `lock()` poll on one
/// thread never starves the holder's save on another, few enough to stay unobtrusive.
const WORKER_THREADS: usize = 2;

/// SQLSTATEs meaning "another transaction holds what you need; retry": `lock_not_available`
/// (`lock_timeout` / `NOWAIT`), deadlock, serialization failure (Python `_LOCK_SQLSTATES`).
const CONTENTION_SQLSTATES: [&str; 3] = ["55P03", "40P01", "40001"];

/// Advisory-lock class (first key of the two-key form) of every lock this backend takes, so they
/// never collide with an application's own single-key advisory locks.
pub(super) const ADVISORY_CLASS: i32 = 0x5853_4d31;

/// Longest identifier PostgreSQL keeps (`NAMEDATALEN - 1`).
const MAX_IDENTIFIER_LEN: usize = 63;

/// A pool plus the runtime that drives it.
#[derive(Debug)]
pub(super) struct Db {
    pool: PgPool,
    handle: Handle,
    /// Present when this value created the runtime (and so the pool) itself.
    owned: Option<Runtime>,
}

impl Db {
    /// Connect `url` on a runtime of our own; every connection gets `schema` as its
    /// `search_path` when one is given.
    pub(super) fn connect(
        url: &str,
        schema: Option<&str>,
        max_connections: u32,
        acquire_timeout: Duration,
    ) -> Result<Self> {
        let mut options = PgConnectOptions::from_str(url)
            .map_err(|_| StoreError::Other("invalid PostgreSQL connection URL".into()))?;
        if let Some(schema) = schema {
            options = options.options([("search_path", validate_ident("schema", schema)?)]);
        }
        let runtime = Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("xsm-postgres")
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        let pool = handle
            .block_on(
                PgPoolOptions::new()
                    .max_connections(max_connections.max(1))
                    .acquire_timeout(acquire_timeout)
                    .connect_with(options),
            )
            .map_err(|e| Fail::Db(e).into_store("<db>"))?;
        Ok(Self { pool, handle, owned: Some(runtime) })
    }

    /// Wrap a pool the caller created, driven by the caller's runtime.
    pub(super) fn from_pool(pool: PgPool, handle: Handle) -> Self {
        Self { pool, handle, owned: None }
    }

    pub(super) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Drive `fut` to completion on this thread; failures are mapped for `key`.
    pub(super) fn run<T>(
        &self,
        key: &str,
        fut: impl Future<Output = std::result::Result<T, Fail>>,
    ) -> Result<T> {
        self.handle.block_on(fut).map_err(|fail| fail.into_store(key))
    }

    /// Like [`run`](Self::run) but hands back the raw [`Fail`] (for `health`, which reports a
    /// label instead of an error).
    pub(super) fn run_raw<T>(
        &self,
        fut: impl Future<Output = std::result::Result<T, Fail>>,
    ) -> std::result::Result<T, Fail> {
        self.handle.block_on(fut)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // A borrowed pool belongs to the caller: closing it here would close it for them.
        let Some(runtime) = self.owned.take() else { return };
        let pool = self.pool.clone();
        if Handle::try_current().is_err() {
            // A plain thread: say goodbye to the server properly.
            self.handle.block_on(pool.close());
        }
        // Never blocks, so dropping the store inside async code cannot panic.
        runtime.shutdown_background();
    }
}

/// Internal failure: a store-level error or a raw sqlx one still to be classified.
#[derive(Debug)]
pub(super) enum Fail {
    Store(StoreError),
    Db(sqlx::Error),
}

impl From<StoreError> for Fail {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

pub(super) type FailResult<T> = std::result::Result<T, Fail>;

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(DatabaseError::code).map(Cow::into_owned)
}

/// `true` for a `unique_violation` (SQLSTATE 23505).
pub(super) fn is_unique_violation(e: &sqlx::Error) -> bool {
    sqlstate(e).as_deref() == Some("23505")
}

impl Fail {
    /// Map to the store taxonomy: contention is a retryable [`StoreError::LockTimeout`], a value
    /// PostgreSQL refuses (SQLSTATE class 22) is [`StoreError::Other`], a column that does not
    /// decode is [`StoreError::Corrupt`], everything else [`StoreError::Unavailable`] (never a
    /// leaked raw driver error).
    pub(super) fn into_store(self, key: &str) -> StoreError {
        let e = match self {
            Self::Store(e) => return e,
            Self::Db(e) => e,
        };
        match sqlstate(&e) {
            Some(code) if CONTENTION_SQLSTATES.contains(&code.as_str()) => {
                return StoreError::LockTimeout {
                    key: key.to_owned(),
                    timeout: Duration::ZERO,
                    holder: None,
                };
            }
            Some(code) if code.starts_with("22") => {
                return StoreError::Other(format!("PostgreSQL rejected the value: {e}"));
            }
            _ => {}
        }
        match e {
            sqlx::Error::ColumnDecode { .. }
            | sqlx::Error::Decode(_)
            | sqlx::Error::ColumnNotFound(_)
            | sqlx::Error::TypeNotFound { .. } => {
                StoreError::Corrupt(format!("PostgresStore: unreadable column: {e}"))
            }
            other => StoreError::Unavailable(format!("PostgresStore: {other}")),
        }
    }

    /// The failure kind only (no host, no SQL): what `health()` reports.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Store(_) => "StoreError",
            Self::Db(sqlx::Error::Io(_)) => "Io",
            Self::Db(sqlx::Error::Tls(_)) => "Tls",
            Self::Db(sqlx::Error::PoolTimedOut) => "PoolTimedOut",
            Self::Db(sqlx::Error::PoolClosed) => "PoolClosed",
            Self::Db(sqlx::Error::Database(_)) => "DatabaseError",
            Self::Db(_) => "Error",
        }
    }
}

/// Validate an identifier we splice into SQL (table or schema name): `[A-Za-z_][A-Za-z0-9_]*`,
/// at most 63 bytes. Returns it unchanged.
pub(super) fn validate_ident<'a>(what: &str, name: &'a str) -> Result<&'a str> {
    let mut chars = name.chars();
    let starts_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_ok
        || name.len() > MAX_IDENTIFIER_LEN
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(StoreError::Other(format!(
            "invalid {what} name {name:?}: use letters, digits and underscores (at most \
             {MAX_IDENTIFIER_LEN}), not starting with a digit"
        )));
    }
    Ok(name)
}

/// Refuse text PostgreSQL cannot store (`NUL`) before it reaches the driver.
pub(super) fn reject_nul(what: &str, text: &str) -> Result<()> {
    if text.contains('\0') {
        return Err(StoreError::InvalidKey(format!("{what} must not contain NUL")));
    }
    Ok(())
}

/// 128 random bits as 32 hex digits (Python `uuid4().hex`'s width; fits `VARCHAR(64)`).
pub(super) fn new_token() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut bytes);
    hex::encode(bytes)
}
