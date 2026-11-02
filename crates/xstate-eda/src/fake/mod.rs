//! In-memory brokers for tests (Python `eda/fake.py`): [`FakeBrokerAdapter`] (async) and
//! [`SyncFakeBrokerAdapter`] share one thread-safe core.
//!
//! Semantics are the broker contract, no more: FIFO per topic (hence per subject), explicit
//! ack / nack, requeue to the head with `attempt + 1` stamped. Envelopes cross the fake the way
//! they cross a real broker: encoded with [`Envelope::to_json`] and decoded again, so an
//! oversized envelope fails at `publish` / `deliver`, and a consumer that mutates its delivery
//! cannot rewrite the `published` record or a redelivery.
//!
//! Test affordances: [`published`](FakeCore::published), [`deliver`](FakeCore::deliver) to
//! inject inbound traffic, [`fail_next_publish`](FakeCore::fail_next_publish) for failure
//! injection, [`on`](FakeCore::on) + [`drain`](FakeCore::drain) to run handlers synchronously.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::broker::Delivery;
use crate::envelope::{DEFAULT_MAX_SNAPSHOT_BYTES, Envelope};
use crate::error::{BrokerError, EdaError, Result};

mod adapters;

pub use adapters::{FakeBrokerAdapter, SyncFakeBrokerAdapter};

/// `drain()` refuses to loop forever: a handler that republishes to its own topic is a test bug,
/// reported loudly instead of hanging CI.
pub const DEFAULT_DRAIN_LIMIT: usize = 100_000;

/// Handler registered with [`FakeCore::on`]; an `Err` nacks the delivery without requeue.
pub type DrainHandler = Arc<dyn Fn(&Envelope) -> std::result::Result<(), String> + Send + Sync>;

static NEXT_BROKER: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct State {
    queues: BTreeMap<String, VecDeque<Envelope>>,
    inflight: HashMap<u64, (Delivery, Envelope)>,
    next_tag: u64,
    published: Vec<Envelope>,
    log: Vec<(String, Envelope)>,
    acked: Vec<Envelope>,
    nacked: Vec<Envelope>,
    fail: VecDeque<BrokerError>,
    handlers: Vec<(String, DrainHandler)>,
}

/// The broker state shared by both fakes. Thread-safe. Records (`published` / `acked` /
/// `nacked`) grow by design; [`clear`](Self::clear) resets everything.
pub struct FakeCore {
    id: u64,
    max_bytes: usize,
    state: Mutex<State>,
}

impl std::fmt::Debug for FakeCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeCore")
            .field("id", &self.id)
            .field("max_bytes", &self.max_bytes)
            .field("in_flight", &self.in_flight())
            .finish_non_exhaustive()
    }
}

impl Default for FakeCore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SNAPSHOT_BYTES)
    }
}

impl FakeCore {
    /// A core whose wire refuses envelopes over `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            id: NEXT_BROKER.fetch_add(1, Ordering::Relaxed),
            max_bytes,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The wire size cap.
    #[must_use]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    // -- test affordances ---------------------------------------------------------------

    /// Make the next `times` publishes (any topic, in call order) fail with `error` (default
    /// [`BrokerError::Publish`]`("injected")`).
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] when `times` is 0.
    pub fn fail_next_publish(&self, error: Option<BrokerError>, times: usize) -> Result<()> {
        if times < 1 {
            return Err(EdaError::InvalidArgument(format!(
                "times must be an int >= 1, got {times}"
            )));
        }
        let error = error.unwrap_or_else(|| BrokerError::Publish("injected".into()));
        let mut st = self.state();
        st.fail.extend(std::iter::repeat_n(error, times));
        Ok(())
    }

    /// Forget everything: queues, records, pending failures, handlers. In-flight deliveries
    /// become unknown (settling them is a no-op).
    pub fn clear(&self) {
        *self.state() = State::default();
    }

    /// Everything ever published (a second decoded copy: compare by id / value).
    #[must_use]
    pub fn published(&self) -> Vec<Envelope> {
        self.state().published.clone()
    }

    /// Everything published on `topic`.
    #[must_use]
    pub fn published_on(&self, topic: &str) -> Vec<Envelope> {
        self.state().log.iter().filter(|(t, _)| t == topic).map(|(_, e)| e.clone()).collect()
    }

    /// Envelopes acked so far.
    #[must_use]
    pub fn acked(&self) -> Vec<Envelope> {
        self.state().acked.clone()
    }

    /// Envelopes nacked so far.
    #[must_use]
    pub fn nacked(&self) -> Vec<Envelope> {
        self.state().nacked.clone()
    }

    /// Envelopes waiting on `topic`.
    #[must_use]
    pub fn pending(&self, topic: &str) -> usize {
        self.state().queues.get(topic).map_or(0, VecDeque::len)
    }

    /// Topics that have (or had) a queue, sorted.
    #[must_use]
    pub fn topics(&self) -> Vec<String> {
        self.state().queues.keys().cloned().collect()
    }

    /// Deliveries taken and not settled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.state().inflight.len()
    }

    /// Register the handler [`drain`](Self::drain) calls for each envelope on `topic`.
    pub fn on(
        &self,
        topic: &str,
        handler: impl Fn(&Envelope) -> std::result::Result<(), String> + Send + Sync + 'static,
    ) {
        let mut st = self.state();
        let handler: DrainHandler = Arc::new(handler);
        if let Some(slot) = st.handlers.iter_mut().find(|(t, _)| t == topic) {
            slot.1 = handler;
        } else {
            st.handlers.push((topic.to_owned(), handler));
        }
    }

    /// Run registered handlers until their topics are empty; returns the envelopes handled.
    ///
    /// A handler that returns `Ok` acks; one that returns `Err` nacks without requeue and the
    /// error propagates. A handler that panics releases its delivery untouched (no nack
    /// counted, `attempt` unchanged) and the panic resumes. Handlers may publish; their output
    /// is drained in the same call. With `topic`, only that topic's handler runs.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for `limit == 0`; [`EdaError::Handler`] from a handler;
    /// [`EdaError::DrainLimit`] when taking one more envelope would exceed `limit`.
    pub fn drain(&self, topic: Option<&str>, limit: usize) -> Result<usize> {
        if limit < 1 {
            return Err(EdaError::InvalidArgument(format!(
                "drain(): limit must be an int >= 1, got {limit}"
            )));
        }
        let mut handled = 0;
        loop {
            let handlers = self.state().handlers.clone();
            let mut progressed = false;
            for (t, handler) in handlers.iter().filter(|(t, _)| topic.is_none_or(|w| w == t)) {
                if handled >= limit {
                    if self.pending(t) == 0 {
                        continue;
                    }
                    return Err(EdaError::DrainLimit { handled });
                }
                let Some(d) = self.take(t) else { continue };
                progressed = true;
                match catch_unwind(AssertUnwindSafe(|| handler(&d.envelope))) {
                    Ok(Ok(())) => self.settle(&d, true, false),
                    Ok(Err(e)) => {
                        self.settle(&d, false, false);
                        return Err(EdaError::Handler(e));
                    }
                    Err(panic) => {
                        self.release(&d);
                        resume_unwind(panic);
                    }
                }
                handled += 1;
            }
            if !progressed {
                return Ok(handled);
            }
        }
    }

    // -- primitives -----------------------------------------------------------------------

    /// Encode + decode, as a real broker would.
    fn wire(&self, envelope: &Envelope) -> std::result::Result<Envelope, BrokerError> {
        let text = envelope.to_json(self.max_bytes)?;
        Ok(Envelope::from_json(&text, self.max_bytes)?)
    }

    /// Publish: the injected failure wins over the size cap (a real client fails on connect
    /// before it serialises).
    pub(crate) fn do_publish(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> std::result::Result<(), BrokerError> {
        if let Some(err) = self.state().fail.pop_front() {
            return Err(err);
        }
        let wired = self.wire(envelope)?;
        let record = self.wire(&wired)?;
        let mut st = self.state();
        st.published.push(record);
        st.log.push((topic.to_owned(), wired.clone()));
        st.queues.entry(topic.to_owned()).or_default().push_back(wired);
        Ok(())
    }

    /// Inject an inbound envelope (not recorded in `published`).
    ///
    /// # Errors
    /// The envelope cannot cross the wire (too large).
    pub fn deliver(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> std::result::Result<(), BrokerError> {
        let wired = self.wire(envelope)?;
        self.state().queues.entry(topic.to_owned()).or_default().push_back(wired);
        Ok(())
    }

    /// Take the head of `topic` as a new in-flight delivery (each delivery gets its own
    /// decoded copy).
    pub(crate) fn take(&self, topic: &str) -> Option<Delivery> {
        let mut st = self.state();
        let pristine = st.queues.get_mut(topic)?.pop_front()?;
        let envelope = self.wire(&pristine).unwrap_or_else(|_| pristine.clone());
        st.next_tag += 1;
        let d =
            Delivery { envelope, topic: topic.to_owned(), tag: st.next_tag, broker_id: self.id };
        st.inflight.insert(d.tag, (d.clone(), pristine));
        Some(d)
    }

    /// Settle a delivery; unknown or already-settled deliveries are a no-op. A requeue
    /// redelivers the pristine envelope with `attempt + 1` at the head of the topic.
    pub(crate) fn settle(&self, d: &Delivery, ack: bool, requeue: bool) {
        if d.broker_id != self.id {
            return;
        }
        let mut st = self.state();
        let Some((held, pristine)) = st.inflight.remove(&d.tag) else { return };
        if ack {
            st.acked.push(held.envelope);
            return;
        }
        st.nacked.push(held.envelope);
        if requeue {
            let again = pristine.with_attempt(pristine.attempt() + 1);
            st.queues.entry(held.topic).or_default().push_front(again);
        }
    }

    /// Put an unsettled delivery back at the head untouched (neither ack nor nack).
    fn release(&self, d: &Delivery) {
        let mut st = self.state();
        if let Some((held, pristine)) = st.inflight.remove(&d.tag) {
            st.queues.entry(held.topic).or_default().push_front(pristine);
        }
    }
}
