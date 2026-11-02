//! The bookkeeping shared by the sync and async adapter: the per-topic local queue, in-flight
//! deliveries, attempt counting and undecodable messages.
//!
//! Python reference: `contrib/brokers/_base.py` (`_Core`, `SyncBroker`, `AsyncBroker`). The core
//! owns what the broker contract promises, identically for every broker:
//!
//! * a per-topic **local queue**: fetched messages wait there, and `nack(requeue = true)` puts
//!   the delivery back at its head, so it is redelivered before anything fetched after it
//!   (per-subject order) without a round trip. The native entry stays un-acked meanwhile, so a
//!   crash still means native redelivery (at-least-once);
//! * **settle once**: the second `ack` / `nack` of a delivery is a no-op;
//! * **attempts**: the broker's own delivery count (the pending list's counter) and every local
//!   requeue are stamped with [`Envelope::with_attempt`], so a poison message is dead-lettered
//!   even across consumer restarts. The wire `xsmattempt` extension is producer-controlled and
//!   never trusted;
//! * the **size cap**: inbound bytes go through [`Envelope::from_json_bytes`] with `max_bytes`
//!   before parsing; an undecodable message is reported and acked, so it can never loop;
//! * **health**: a transport call that fails marks the adapter unhealthy until one succeeds.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::UndecodableHook;
use super::transport::{Native, Raw, Transport};
use crate::broker::Delivery;
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// Longest single blocking fetch when `poll` is given no deadline (Python `POLL_S`).
pub(super) const POLL_SLICE: Duration = Duration::from_secs(1);

static NEXT_BROKER: AtomicU64 = AtomicU64::new(1);

/// A fetched message waiting in the local queue.
#[derive(Debug)]
struct Local {
    envelope: Envelope,
    native: Native,
    at: Instant,
}

/// A delivery handed out and not yet settled; the envelope is the pristine copy a requeue
/// redelivers, whatever the consumer did to its own.
#[derive(Debug)]
struct Inflight {
    topic: String,
    envelope: Envelope,
    native: Native,
    at: Instant,
}

#[derive(Debug, Default)]
struct State {
    local: HashMap<String, VecDeque<Local>>,
    inflight: HashMap<u64, Inflight>,
    next_tag: u64,
}

/// Everything the adapter needs, shareable between threads and tasks.
pub(super) struct Core {
    id: u64,
    transport: Transport,
    on_undecodable: Option<UndecodableHook>,
    state: Mutex<State>,
    healthy: AtomicBool,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("id", &self.id)
            .field("healthy", &self.healthy.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The instant `timeout` from now; `None` for no deadline or one too far away to represent.
pub(super) fn deadline_after(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

/// `true` once `deadline` has passed.
pub(super) fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// How long the next fetch may block: at most [`POLL_SLICE`], never past `deadline`.
pub(super) fn wait_for(deadline: Option<Instant>) -> Duration {
    deadline.map_or(POLL_SLICE, |d| POLL_SLICE.min(d.saturating_duration_since(Instant::now())))
}

impl Core {
    pub(super) fn new(transport: Transport, on_undecodable: Option<UndecodableHook>) -> Self {
        Self {
            id: NEXT_BROKER.fetch_add(1, Ordering::Relaxed),
            transport,
            on_undecodable,
            state: Mutex::new(State::default()),
            healthy: AtomicBool::new(true),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `false` after a transport call failed, until one succeeds.
    pub(super) fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Deliveries handed out and not yet settled.
    pub(super) fn in_flight(&self) -> usize {
        self.state().inflight.len()
    }

    /// Messages fetched (or requeued) locally and not yet delivered on `topic`.
    pub(super) fn held(&self, topic: &str) -> usize {
        self.state().local.get(topic).map_or(0, VecDeque::len)
    }

    /// Run a transport call and record whether it worked. Only the error type is logged: client
    /// errors may embed credentials.
    fn call<T>(
        &self,
        op: impl FnOnce(&Transport) -> Result<T, BrokerError>,
    ) -> Result<T, BrokerError> {
        let result = op(&self.transport);
        let was_healthy = self.healthy.swap(result.is_ok(), Ordering::Relaxed);
        if result.is_err() && was_healthy {
            tracing::warn!("broker call failed; the Redis Streams adapter is unhealthy");
        } else if result.is_ok() && !was_healthy {
            tracing::info!("broker connection healthy again");
        }
        result
    }

    // -- publish and settle ---------------------------------------------------------------

    pub(super) fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        self.call(|t| t.send(topic, envelope))
    }

    /// Settle a delivery as processed (a delivery of another broker, or one already settled, is
    /// a no-op).
    pub(super) fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        match self.claim(delivery) {
            Some(held) => self.call(|t| t.ack(&held.native)),
            None => Ok(()),
        }
    }

    /// Requeue the delivery locally at the head of its topic (`attempt + 1`), or drop it with a
    /// native ack: dead-lettering is the dispatcher's job.
    pub(super) fn nack(&self, delivery: &Delivery, requeue: bool) -> Result<(), BrokerError> {
        let Some(held) = self.claim(delivery) else { return Ok(()) };
        if !requeue {
            return self.call(|t| t.ack(&held.native));
        }
        let again = held.envelope.with_attempt(held.envelope.attempt() + 1);
        self.state().local.entry(held.topic).or_default().push_front(Local {
            envelope: again,
            native: held.native,
            at: held.at,
        });
        Ok(())
    }

    fn claim(&self, delivery: &Delivery) -> Option<Inflight> {
        if delivery.broker_id != self.id {
            return None;
        }
        self.state().inflight.remove(&delivery.tag)
    }

    // -- receive --------------------------------------------------------------------------

    /// The next message of the local queue as a delivery, without blocking. Entries held past
    /// half the redelivery window are released instead: forgotten locally without a native ack,
    /// so the broker redelivers them on its own clock rather than this process delivering them
    /// too late (which would inflate attempts and dead-letter healthy messages).
    pub(super) fn pop(&self, topic: &str) -> Option<Delivery> {
        let hold = self.transport.redelivery_window() / 2;
        let mut released: Vec<Native> = Vec::new();
        let delivery = {
            let mut st = self.state();
            let mut next = None;
            while let Some(entry) = st.local.get_mut(topic).and_then(VecDeque::pop_front) {
                if !hold.is_zero() && entry.at.elapsed() > hold {
                    released.push(entry.native);
                } else {
                    next = Some(entry);
                    break;
                }
            }
            next.map(|entry| self.hand_out(&mut st, topic, entry))
        };
        for native in &released {
            tracing::debug!("released a locally expired message on {topic:?}");
            self.transport.forget(native);
        }
        delivery
    }

    fn hand_out(&self, st: &mut State, topic: &str, entry: Local) -> Delivery {
        st.next_tag += 1;
        let tag = st.next_tag;
        st.inflight.insert(
            tag,
            Inflight {
                topic: topic.to_owned(),
                envelope: entry.envelope.clone(),
                native: entry.native,
                at: entry.at,
            },
        );
        Delivery { envelope: entry.envelope, topic: topic.to_owned(), tag, broker_id: self.id }
    }

    /// Fetch from Redis (blocking up to `wait`) and put the decodable messages in the local
    /// queue; the undecodable ones are reported and acked. Landing the messages in the queue
    /// instead of returning them means a caller that gives up mid-fetch (a cancelled task)
    /// loses nothing: the next `pop` delivers them.
    pub(super) fn fill(&self, topic: &str, wait: Duration) -> Result<(), BrokerError> {
        let raws = self.call(|t| t.fetch(topic, wait))?;
        let mut good = Vec::new();
        let mut bad = Vec::new();
        for raw in raws {
            if let Some(envelope) = self.decode(topic, &raw) {
                good.push(Local { envelope, native: raw.native, at: Instant::now() });
            } else {
                bad.push(raw.native);
            }
        }
        self.state().local.entry(topic.to_owned()).or_default().extend(good);
        // after stashing: a failing ack loses none of the good ones
        for native in &bad {
            self.call(|t| t.ack(native))?;
        }
        Ok(())
    }

    /// One fetched message as an envelope, or `None` (reported) when it cannot be decoded. The
    /// attempt is the broker's count only.
    fn decode(&self, topic: &str, raw: &Raw) -> Option<Envelope> {
        let max_bytes = self.transport.settings.max_bytes;
        match Envelope::from_json_bytes(&raw.body, max_bytes) {
            Ok(envelope) if envelope.attempt() == raw.attempts => Some(envelope),
            Ok(envelope) => Some(envelope.with_attempt(raw.attempts)),
            Err(error) => {
                // the type only, never the body: it may be hostile or sensitive
                tracing::warn!(
                    "dropping an undecodable message on {topic:?}: {}",
                    error.type_name()
                );
                if let Some(hook) = &self.on_undecodable {
                    hook(topic, &error);
                }
                None
            }
        }
    }
}
