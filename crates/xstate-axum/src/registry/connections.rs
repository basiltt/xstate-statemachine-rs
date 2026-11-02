//! Stream connection accounting: a per-instance and a registry-wide cap on concurrent SSE /
//! WebSocket clients, and the abort handles [`StatechartRegistry::shutdown`] uses to end
//! sessions that did not finish on their own.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::AbortHandle;

use super::{StatechartRegistry, relock};

/// Default registry-wide cap on concurrent stream clients.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// How often the drain re-checks whether every stream ended.
const DRAIN_POLL: Duration = Duration::from_millis(10);

type Topic = (String, String);

/// Open stream connections: per instance, in total, and the sessions to abort on shutdown.
#[derive(Debug, Default)]
pub(crate) struct Connections {
    per_key: HashMap<Topic, usize>,
    total: usize,
    next_id: u64,
    sessions: HashMap<u64, AbortHandle>,
}

/// Why [`StatechartRegistry::try_open_connection`] refused a stream slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionRefused {
    /// The registry is draining (SSE 503, WebSocket close 1001).
    Draining,
    /// `max_connections_per_key` reached for this instance (SSE 429, WebSocket close 1013).
    PerKey,
    /// `max_connections` reached across the registry (SSE 429, WebSocket close 1013).
    Global,
}

impl StatechartRegistry {
    /// Reserve a stream slot for an instance, enforcing both
    /// [`max_connections_per_key`](super::RegistryBuilder::max_connections_per_key) and
    /// [`max_connections`](super::RegistryBuilder::max_connections). The slot is released when
    /// the returned guard drops.
    ///
    /// # Errors
    /// [`ConnectionRefused`] saying which limit refused (or that the registry is draining).
    pub fn try_open_connection(
        self: &Arc<Self>,
        name: &str,
        key: &str,
    ) -> Result<ConnectionSlot, ConnectionRefused> {
        let topic = (name.to_owned(), key.to_owned());
        let mut conns = relock(&self.connections);
        if self.is_draining() {
            return Err(ConnectionRefused::Draining);
        }
        let n = conns.per_key.get(&topic).copied().unwrap_or(0);
        if n >= self.settings.max_connections_per_key {
            return Err(ConnectionRefused::PerKey);
        }
        if conns.total >= self.settings.max_connections {
            return Err(ConnectionRefused::Global);
        }
        conns.per_key.insert(topic.clone(), n + 1);
        conns.total += 1;
        conns.next_id += 1;
        let id = conns.next_id;
        Ok(ConnectionSlot { registry: Arc::clone(self), topic, id })
    }

    fn close_connection(&self, topic: &Topic, id: u64) {
        let mut conns = relock(&self.connections);
        conns.sessions.remove(&id);
        conns.total = conns.total.saturating_sub(1);
        match conns.per_key.get(topic).copied() {
            Some(n) if n > 1 => {
                conns.per_key.insert(topic.clone(), n - 1);
            }
            _ => {
                conns.per_key.remove(topic);
            }
        }
    }

    /// Open stream connections of one instance, or of every instance when `topic` is `None`.
    #[must_use]
    pub fn connections(&self, topic: Option<(&str, &str)>) -> usize {
        let conns = relock(&self.connections);
        match topic {
            None => conns.total,
            Some((name, key)) => {
                conns.per_key.get(&(name.to_owned(), key.to_owned())).copied().unwrap_or(0)
            }
        }
    }

    /// Shutdown step: wait until `deadline` for the streams (already told to end by the
    /// fan-out) to close, then abort the sessions still open (a stuck client, a send in
    /// flight).
    pub(super) async fn end_sessions(&self, deadline: tokio::time::Instant) {
        while self.connections(None) > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        let stuck: Vec<AbortHandle> =
            relock(&self.connections).sessions.values().cloned().collect();
        if !stuck.is_empty() {
            tracing::warn!(target: "xstate::axum", sessions = stuck.len(), "aborting stream sessions still open after the drain timeout");
        }
        for handle in stuck {
            handle.abort();
        }
    }
}

/// A reserved stream slot (SSE or WebSocket); released on drop.
pub struct ConnectionSlot {
    registry: Arc<StatechartRegistry>,
    topic: Topic,
    id: u64,
}

impl ConnectionSlot {
    /// Let [`StatechartRegistry::shutdown`] abort the session holding this slot.
    pub(crate) fn attach(&self, handle: AbortHandle) {
        relock(&self.registry.connections).sessions.insert(self.id, handle);
    }
}

impl fmt::Debug for ConnectionSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionSlot").field("topic", &self.topic).finish_non_exhaustive()
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.registry.close_connection(&self.topic, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegisterOptions, allow_all};

    fn registry(
        f: impl FnOnce(crate::RegistryBuilder) -> crate::RegistryBuilder,
    ) -> Arc<StatechartRegistry> {
        let machine =
            xstate_core::Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{}}}"#)
                .unwrap_or_else(|e| panic!("{e}"));
        let store: Arc<dyn xstate_persistence::StateStore> =
            Arc::new(xstate_persistence::MemoryStore::new());
        // allow_all: no authorizer under test here.
        f(StatechartRegistry::builder(store))
            .register(
                "m",
                machine,
                xstate_core::MachineLogic::dynamic(),
                RegisterOptions::new(allow_all()),
            )
            .and_then(crate::RegistryBuilder::build)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn per_key_and_global_caps() {
        let reg = registry(|b| b.max_connections_per_key(1).max_connections(2));
        let a = reg.try_open_connection("m", "a");
        assert!(a.is_ok());
        assert_eq!(reg.try_open_connection("m", "a").err(), Some(ConnectionRefused::PerKey));
        let b = reg.try_open_connection("m", "b");
        assert!(b.is_ok());
        assert_eq!(reg.try_open_connection("m", "c").err(), Some(ConnectionRefused::Global));
        assert_eq!(reg.connections(None), 2);
        drop(a);
        assert_eq!(reg.connections(None), 1);
        assert!(reg.try_open_connection("m", "c").is_ok(), "a freed slot is reusable");
        drop(b);
    }

    #[tokio::test]
    async fn shutdown_aborts_sessions_that_do_not_end() {
        let reg = registry(|b| b.drain_timeout(Duration::from_millis(50)));
        let slot = reg.try_open_connection("m", "k").unwrap_or_else(|e| panic!("{e:?}"));
        let (handle, registration) = AbortHandle::new_pair();
        slot.attach(handle);
        let stuck = futures_util::future::Abortable::new(
            async move {
                let _slot = slot;
                std::future::pending::<()>().await;
            },
            registration,
        );
        let task = tokio::spawn(stuck);
        reg.shutdown().await;
        let outcome = task.await.unwrap_or_else(|e| panic!("{e}"));
        assert!(outcome.is_err(), "the session was aborted");
        assert_eq!(reg.connections(None), 0);
        assert_eq!(reg.try_open_connection("m", "k").err(), Some(ConnectionRefused::Draining));
    }
}
