//! In-process transition fan-out for SSE / WebSocket subscribers (Python `_fanout.py`).
//!
//! The registry publishes **after** a send's save committed, so a subscriber never sees a
//! transition that a conflict rolled back. Strictly per-process: with N workers a client only
//! hears transitions made by the worker it is connected to.
//!
//! * Each subscriber owns a bounded queue of [`MAX_BACKLOG`] items. A consumer that falls that
//!   far behind is cut off: it receives what is queued, then [`FanoutItem::Closed`]; the
//!   publisher never blocks.
//! * Each topic (machine name, instance key) carries a monotonic `seq` stamped on every
//!   published body. The counter lives only while somebody listens; a reconnecting client
//!   starts from a fresh `snapshot` frame (`Last-Event-ID` is not replayed, as in Python).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tokio::sync::mpsc;

/// Per-subscriber backlog; a consumer this far behind is cut off.
pub const MAX_BACKLOG: usize = 256;

type Topic = (String, String);

/// What a subscriber receives.
#[derive(Debug, Clone, PartialEq)]
pub enum FanoutItem {
    /// One committed transition: its sequence number and receipt body.
    Transition {
        /// Per-instance increasing sequence number (SSE `id:`).
        seq: u64,
        /// The receipt body (`receipt_body` shape).
        body: Arc<Value>,
    },
    /// The stream is over: registry shutdown, or this consumer lagged and was cut off.
    Closed,
}

#[derive(Debug, Default)]
struct TopicState {
    seq: u64,
    subs: Vec<(u64, mpsc::Sender<FanoutItem>)>,
}

#[derive(Debug, Default)]
struct Inner {
    topics: HashMap<Topic, TopicState>,
    next_id: u64,
}

/// Topic → subscribers, plus a per-topic sequence counter.
#[derive(Debug, Default)]
pub struct Subscribers {
    inner: Mutex<Inner>,
}

impl Subscribers {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add a subscriber for one instance; dropping the [`Subscription`] removes it.
    #[must_use]
    pub fn subscribe(self: &Arc<Self>, name: &str, key: &str) -> Subscription {
        let (tx, rx) = mpsc::channel(MAX_BACKLOG);
        let topic = (name.to_owned(), key.to_owned());
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.topics.entry(topic.clone()).or_default().subs.push((id, tx));
            id
        };
        Subscription { subscribers: Arc::clone(self), topic, id, rx }
    }

    fn unsubscribe(&self, topic: &Topic, id: u64) {
        let mut inner = self.lock();
        if let Some(state) = inner.topics.get_mut(topic) {
            state.subs.retain(|(sid, _)| *sid != id);
            if state.subs.is_empty() {
                // The sequence is only meaningful to a connected client (battle #275).
                inner.topics.remove(topic);
            }
        }
    }

    /// Count subscribers of one instance, or of every instance when `topic` is `None`.
    #[must_use]
    pub fn count(&self, topic: Option<(&str, &str)>) -> usize {
        let inner = self.lock();
        match topic {
            None => inner.topics.values().map(|s| s.subs.len()).sum(),
            Some((name, key)) => {
                inner.topics.get(&(name.to_owned(), key.to_owned())).map_or(0, |s| s.subs.len())
            }
        }
    }

    /// The current sequence number of an instance (0 when nobody listens).
    #[must_use]
    pub fn seq(&self, name: &str, key: &str) -> u64 {
        self.lock().topics.get(&(name.to_owned(), key.to_owned())).map_or(0, |s| s.seq)
    }

    /// Stamp each body with the next `seq` and enqueue it for every subscriber of the instance.
    /// A subscriber whose queue is full is cut off (never blocks the writer). Nobody
    /// listening: nothing happens and no sequence advances.
    pub fn publish(&self, name: &str, key: &str, bodies: Vec<Value>) {
        let mut inner = self.lock();
        let Some(state) = inner.topics.get_mut(&(name.to_owned(), key.to_owned())) else {
            return;
        };
        for body in bodies {
            state.seq += 1;
            let item = FanoutItem::Transition { seq: state.seq, body: Arc::new(body) };
            state.subs.retain(|(_, tx)| match tx.try_send(item.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(target: "xstate::axum", %name, %key, "slow stream consumer cut off");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
        }
        if state.subs.is_empty() {
            inner.topics.remove(&(name.to_owned(), key.to_owned()));
        }
    }

    /// End every stream (shutdown): subscribers drain what is queued, then see
    /// [`FanoutItem::Closed`].
    pub fn close_all(&self) {
        self.lock().topics.clear();
    }
}

/// One subscriber's end of the fan-out; unsubscribes on drop.
#[derive(Debug)]
pub struct Subscription {
    subscribers: Arc<Subscribers>,
    topic: Topic,
    id: u64,
    rx: mpsc::Receiver<FanoutItem>,
}

impl Subscription {
    /// The next item; [`FanoutItem::Closed`] once the stream is over.
    pub async fn recv(&mut self) -> FanoutItem {
        self.rx.recv().await.unwrap_or(FanoutItem::Closed)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.subscribers.unsubscribe(&self.topic, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_stamps_sequence_and_cuts_slow_consumers() {
        let subs = Arc::new(Subscribers::new());
        subs.publish("m", "k", vec![Value::Null]);
        assert_eq!(subs.seq("m", "k"), 0, "nobody listening: no sequence");
        let mut fast = subs.subscribe("m", "k");
        subs.publish("m", "k", vec![Value::from(1)]);
        assert!(matches!(fast.recv().await, FanoutItem::Transition { seq: 1, .. }));
        let mut slow = subs.subscribe("m", "k");
        subs.publish("m", "k", vec![Value::Null; MAX_BACKLOG + 1]);
        let mut received = 0;
        while let FanoutItem::Transition { .. } = slow.recv().await {
            received += 1;
        }
        assert_eq!(received, MAX_BACKLOG, "backlog delivered, then closed");
        assert_eq!(subs.count(Some(("m", "k"))), 0, "fast consumer was cut too");
        drop(fast);
        assert_eq!(subs.count(None), 0);
    }

    #[tokio::test]
    async fn close_all_ends_streams_and_drop_unsubscribes() {
        let subs = Arc::new(Subscribers::new());
        let mut a = subs.subscribe("m", "k");
        let b = subs.subscribe("m", "j");
        assert_eq!(subs.count(None), 2);
        drop(b);
        assert_eq!(subs.count(None), 1);
        subs.close_all();
        assert_eq!(a.recv().await, FanoutItem::Closed);
    }
}
