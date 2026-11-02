//! A small pool of blocking Redis connections for the stream broker.

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use redis::{Client, Connection, RedisError, RedisResult};

use crate::error::BrokerError;

/// Idle connections kept for reuse.
const MAX_IDLE: usize = 8;

/// Connections to one Redis. The URL is never kept in a message or `Debug` output: it may
/// carry a password.
pub(super) struct Pool {
    client: Client,
    idle: Mutex<Vec<Connection>>,
    /// Bound on connecting and on every socket read / write.
    io_timeout: Duration,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The failure is the connection's, not the command's.
fn is_transport(e: &RedisError) -> bool {
    e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() || e.is_timeout()
}

/// A failed Redis call as a broker error (no URL, no payload).
pub(super) fn broker_error(e: &RedisError) -> BrokerError {
    BrokerError::Other(format!("redis: {e}"))
}

impl Pool {
    /// Parse `url`; no connection is made yet.
    pub(super) fn open(url: &str, io_timeout: Duration) -> Result<Self, String> {
        let client =
            Client::open(url).map_err(|e| format!("invalid Redis URL ({:?})", e.kind()))?;
        Ok(Self { client, idle: Mutex::new(Vec::new()), io_timeout })
    }

    /// The bound on connecting and on every socket read / write.
    pub(super) fn io_timeout(&self) -> Duration {
        self.io_timeout
    }

    fn checkout(&self) -> RedisResult<(Connection, bool)> {
        if let Some(conn) = relock(&self.idle).pop() {
            return Ok((conn, true));
        }
        let conn = self.client.get_connection_with_timeout(self.io_timeout)?;
        conn.set_read_timeout(Some(self.io_timeout))?;
        conn.set_write_timeout(Some(self.io_timeout))?;
        Ok((conn, false))
    }

    /// Run `f` on a pooled connection. A transport failure discards the connection (and the idle
    /// ones when it came from the pool: the server probably restarted); a command the server
    /// refused leaves it reusable.
    pub(super) fn run<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> RedisResult<T>,
    ) -> Result<T, BrokerError> {
        let (mut conn, pooled) = self.checkout().map_err(|e| broker_error(&e))?;
        match f(&mut conn) {
            Ok(value) => {
                let mut idle = relock(&self.idle);
                if idle.len() < MAX_IDLE {
                    idle.push(conn);
                }
                Ok(value)
            }
            Err(e) => {
                if !is_transport(&e) {
                    relock(&self.idle).push(conn);
                } else if pooled {
                    relock(&self.idle).clear();
                }
                Err(broker_error(&e))
            }
        }
    }
}
