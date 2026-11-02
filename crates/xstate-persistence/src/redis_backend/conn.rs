//! Blocking connections to Redis: a small pool, the mapping of Redis failures onto
//! [`StoreError`], and the cursor loops shared by the stores.
//!
//! Python reference: `contrib/redis/_errors.py` (`typed`) and `_layout.py`
//! (`_client_from_url`, `DEFAULT_SOCKET_TIMEOUT_S`).

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use redis::{Client, Connection, ErrorKind, RedisError, RedisResult};

use crate::memory_store::relock;
use crate::store::{Result, StoreError};

/// Bound on connecting and on every socket read / write (Python `DEFAULT_SOCKET_TIMEOUT_S`): a
/// Redis that accepts TCP but never answers must not hang a `load()` forever.
pub(crate) const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle connections kept for reuse.
const MAX_IDLE: usize = 8;

/// Keys requested per `SCAN` / `SSCAN` round trip (Python `count=500`).
const SCAN_COUNT: u32 = 500;

/// A pool of blocking connections to one Redis. The URL is never kept in a message or `Debug`
/// output: it may carry a password.
pub(crate) struct Pool {
    client: Client,
    idle: Mutex<Vec<Connection>>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

impl Pool {
    /// Parse `url` (`redis://host:6379/0`, `rediss://` for TLS). No connection is made yet.
    pub(crate) fn open(url: &str) -> Result<Self> {
        let client = Client::open(url)
            .map_err(|e| StoreError::Other(format!("invalid Redis URL ({:?})", e.kind())))?;
        Ok(Self { client, idle: Mutex::new(Vec::new()) })
    }

    fn checkout(&self, connect_timeout: Duration) -> RedisResult<(Connection, bool)> {
        if let Some(conn) = relock(&self.idle).pop() {
            return Ok((conn, true));
        }
        let conn = self.client.get_connection_with_timeout(connect_timeout)?;
        conn.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        conn.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        Ok((conn, false))
    }

    fn give_back(&self, conn: Connection) {
        let mut idle = relock(&self.idle);
        if idle.len() < MAX_IDLE {
            idle.push(conn);
        }
    }

    /// Run `f` on a pooled connection. A connection that failed at the transport level is
    /// discarded, and so are the idle ones when it came from the pool (the server probably
    /// restarted and they are all stale); a command the server refused leaves it reusable.
    pub(crate) fn run<T>(&self, f: impl FnOnce(&mut Connection) -> RedisResult<T>) -> Result<T> {
        self.run_within(SOCKET_TIMEOUT, f)
    }

    /// [`run`](Self::run) with connecting and every socket read / write bounded by `limit`
    /// instead of [`SOCKET_TIMEOUT`] (best-effort work such as releasing a lock from `Drop`,
    /// which must not stall its caller for the full default).
    pub(crate) fn run_within<T>(
        &self,
        limit: Duration,
        f: impl FnOnce(&mut Connection) -> RedisResult<T>,
    ) -> Result<T> {
        let (mut conn, pooled) = self.checkout(limit).map_err(|e| store_error(&e))?;
        let shortened = limit < SOCKET_TIMEOUT;
        if shortened && let Err(e) = set_timeouts(&conn, limit) {
            return Err(store_error(&e));
        }
        let outcome = f(&mut conn);
        let restored = !shortened || set_timeouts(&conn, SOCKET_TIMEOUT).is_ok();
        match outcome {
            Ok(value) => {
                if restored {
                    self.give_back(conn);
                }
                Ok(value)
            }
            Err(e) if is_transport(&e) => {
                if pooled {
                    relock(&self.idle).clear();
                }
                Err(store_error(&e))
            }
            Err(e) => {
                if restored {
                    self.give_back(conn);
                }
                Err(store_error(&e))
            }
        }
    }
}

fn set_timeouts(conn: &Connection, timeout: Duration) -> RedisResult<()> {
    conn.set_read_timeout(Some(timeout))?;
    conn.set_write_timeout(Some(timeout))
}

/// The failure is the connection's, not the command's: nothing about the server's data follows.
fn is_transport(e: &RedisError) -> bool {
    e.is_io_error()
        || e.is_connection_dropped()
        || e.is_connection_refusal()
        || e.is_timeout()
        || e.kind() == ErrorKind::BusyLoadingError
}

/// `unknown command 'evalsha'`: the server has no Lua scripting.
fn lacks_lua(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("unknown command")
        && ["evalsha", "eval", "script"].iter().any(|c| lower.contains(c))
}

/// What a connection-class failure was, in words that carry no address.
fn transport_label(e: &RedisError) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connection_refusal() {
        "connection refused"
    } else if e.is_connection_dropped() {
        "connection dropped"
    } else if e.kind() == ErrorKind::BusyLoadingError {
        "server loading"
    } else {
        "i/o error"
    }
}

/// Map a Redis failure onto the store taxonomy: connection-class failures are
/// [`StoreError::Unavailable`] (retryable, nothing was written), a reply of the wrong shape is
/// [`StoreError::Corrupt`], anything else is [`StoreError::Other`].
///
/// The message is a fixed phrase plus the failure's kind (and the server's error code, a short
/// token such as `WRONGTYPE`): the driver's own text can carry the `host:port`, key names or a
/// URL, and error values travel into logs, receipts and dead letters. The full text is logged
/// at `debug` only.
pub(crate) fn store_error(e: &RedisError) -> StoreError {
    tracing::debug!(error = %e, kind = ?e.kind(), "redis call failed");
    if is_transport(e) {
        return StoreError::Unavailable(format!(
            "redis: backend unavailable ({})",
            transport_label(e)
        ));
    }
    let text = e.to_string();
    if lacks_lua(&text) {
        return StoreError::Other(
            "redis: the server has no Lua scripting (EVALSHA unknown); the Redis backends need \
             Redis >= 7"
                .to_owned(),
        );
    }
    if e.kind() == ErrorKind::TypeError {
        return StoreError::Corrupt("redis: unexpected reply (wrong type)".to_owned());
    }
    match e.code() {
        Some(code) => StoreError::Other(format!("redis: command failed ({:?}, {code})", e.kind())),
        None => StoreError::Other(format!("redis: command failed ({:?})", e.kind())),
    }
}

/// The members matching `pattern`, by walking a cursor: `SSCAN set_key` over a set, or `SCAN`
/// over the keyspace when `set_key` is `None`. A cursor scan may repeat a name; the result is
/// sorted and de-duplicated.
pub(crate) fn scan_all(
    conn: &mut Connection,
    set_key: Option<&str>,
    pattern: &str,
) -> RedisResult<Vec<String>> {
    let mut cursor = 0u64;
    let mut found: Vec<String> = Vec::new();
    loop {
        let mut cmd = redis::cmd(if set_key.is_some() { "SSCAN" } else { "SCAN" });
        if let Some(key) = set_key {
            cmd.arg(key);
        }
        let (next, mut page): (u64, Vec<String>) =
            cmd.arg(cursor).arg("MATCH").arg(pattern).arg("COUNT").arg(SCAN_COUNT).query(conn)?;
        found.append(&mut page);
        if next == 0 {
            found.sort();
            found.dedup();
            return Ok(found);
        }
        cursor = next;
    }
}

/// A short, host-free label of a store error for health reports.
pub(crate) fn error_label(e: &StoreError) -> &'static str {
    match e {
        StoreError::Unavailable(_) => "Unavailable",
        StoreError::Corrupt(_) => "Corrupt",
        _ => "Error",
    }
}
