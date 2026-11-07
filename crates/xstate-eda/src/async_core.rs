//! The bookkeeping shared by the NATS and RabbitMQ adapters: the per-topic local queue, in-flight
//! deliveries, attempt counting, undecodable messages and the owned Tokio runtime both clients
//! run on.
//!
//! Python reference: `contrib/brokers/_base.py` (`_Core`, `SyncBroker`, `AsyncBroker`). The core
//! owns what the broker contract promises, identically for every broker:
//!
//! * a per-topic **local queue**: fetched messages wait there, and `nack(requeue = true)` puts
//!   the delivery back at its head, so it is redelivered before anything fetched after it
//!   (per-subject order) without a round trip. The native message stays un-acked meanwhile, so a
//!   crash still means native redelivery (at-least-once);
//! * **settle once**: the second `ack` / `nack` of a delivery is a no-op;
//! * **attempts**: the broker's own redelivery count and every local requeue are stamped with
//!   [`Envelope::with_attempt`], so a poison message is dead-lettered even across consumer
//!   restarts. The wire `xsmattempt` extension is producer-controlled and never trusted;
//! * the **size cap**: inbound bytes go through [`Envelope::from_json_bytes`] with `max_bytes`
//!   before parsing; an undecodable message is reported and dropped, so it can never loop;
//! * **health**: a transport call that fails marks the adapter unhealthy until one succeeds.
//!
//! The clients ([`async_nats`], [`lapin`]) are asynchronous, so a [`Broker`] owns a small
//! multi-thread Tokio runtime and runs every transport call on it. That makes the async
//! contract runtime-agnostic (the futures only await a join handle) and lets the blocking
//! contract work from plain threads. A poll that is cancelled mid-fetch loses nothing: the
//! fetch keeps running on the owned runtime, lands its messages in the local queue, and the
//! next poll delivers them.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
#[cfg(feature = "sqs")]
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime};

use crate::broker::Delivery;
use crate::envelope::Envelope;
use crate::error::{BrokerError, EdaError, EnvelopeError};

/// Called with the topic and the reason for every inbound message that could not be decoded
/// (the message is dropped without redelivery; its body is never logged).
pub type UndecodableHook = Arc<dyn Fn(&str, &EnvelopeError) + Send + Sync>;

/// Longest single blocking fetch when `poll` is given no deadline (Python `POLL_S`).
const POLL_SLICE: Duration = Duration::from_secs(1);

/// Worker threads of the owned runtime: the clients' I/O needs no more.
const RUNTIME_THREADS: usize = 2;

/// How much longer a blocking call waits than the transport call it runs, so the transport's
/// own timeout (which carries the better error) is the one that fires.
const BLOCK_GRACE: Duration = Duration::from_secs(1);

/// The default bound on one transport call (`io_timeout` of every adapter's options).
pub(crate) const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_BROKER: AtomicU64 = AtomicU64::new(1);

/// A failure while preparing a publish is a publish failure.
pub(crate) fn into_publish(error: BrokerError) -> BrokerError {
    match error {
        BrokerError::Other(why) => BrokerError::Publish(why),
        other => other,
    }
}

/// CloudEvents structured-mode content type (Kafka/AMQP/NATS bindings; SQS has no such header).
#[cfg(any(feature = "nats", feature = "rabbitmq"))]
pub(crate) const CE_CONTENT_TYPE: &str = "application/cloudevents+json; charset=UTF-8";

/// One broker-specific operation on a held message, ready to run (`sqs`).
#[cfg(feature = "sqs")]
type HeldFuture = Pin<Box<dyn Future<Output = Result<(), BrokerError>> + Send>>;

/// One message as a transport fetched it.
#[derive(Debug)]
pub(crate) struct Raw<N> {
    /// The structured-mode CloudEvents JSON.
    pub(crate) body: Vec<u8>,
    /// What the transport needs to settle it later.
    pub(crate) native: N,
    /// Deliveries the broker already made before this one (0 for a first delivery).
    pub(crate) attempts: u64,
}

/// The four native operations of a broker, and nothing else (Python `*Transport`).
pub(crate) trait Transport: Send + Sync + 'static {
    /// The handle that settles one delivery.
    type Native: Send + Sync + 'static;

    /// Adapter name for log lines.
    const NAME: &'static str;

    /// The broker's redelivery clock (ack wait); `ZERO` = none, so locally held messages are
    /// never released.
    fn redelivery_window(&self) -> Duration;

    /// The size cap of an envelope on the wire.
    fn max_bytes(&self) -> usize;

    /// The bound on one native call (connecting, publishing and awaiting the confirmation,
    /// fetching, settling); the core fails the call as `timed out` when it is exceeded. A
    /// fetch may additionally wait as long as it was asked to.
    fn io_timeout(&self) -> Duration;

    /// Publish `envelope` on `topic`.
    fn send(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send;

    /// Pull what is ready on `topic`, waiting at most `wait`.
    fn fetch(
        &self,
        topic: &str,
        wait: Duration,
    ) -> impl Future<Output = Result<Vec<Raw<Self::Native>>, BrokerError>> + Send;

    /// Settle a delivery for good.
    fn ack(&self, native: &Self::Native) -> impl Future<Output = Result<(), BrokerError>> + Send;

    /// Settle a delivery without redelivery.
    fn drop_message(
        &self,
        native: &Self::Native,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send;

    /// A message was released locally without settling it (the broker redelivers it).
    fn forget(&self, _native: &Self::Native) {}

    /// Flush and close the client connection this transport opened.
    fn close(&self) -> impl Future<Output = Result<(), BrokerError>> + Send;
}

/// A fetched message waiting in the local queue.
#[derive(Debug)]
struct Local<N> {
    envelope: Envelope,
    native: N,
    at: Instant,
}

/// A delivery handed out and not yet settled; the envelope is the pristine copy a requeue
/// redelivers, whatever the consumer did to its own.
#[derive(Debug)]
struct Inflight<N> {
    topic: String,
    envelope: Envelope,
    native: N,
    at: Instant,
}

#[derive(Debug)]
struct State<N> {
    local: HashMap<String, VecDeque<Local<N>>>,
    inflight: HashMap<u64, Inflight<N>>,
    next_tag: u64,
}

impl<N> Default for State<N> {
    fn default() -> Self {
        Self { local: HashMap::new(), inflight: HashMap::new(), next_tag: 0 }
    }
}

/// The instant `timeout` from now; `None` for no deadline or one too far away to represent.
fn deadline_after(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

/// `true` once `deadline` has passed.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// How long the next fetch may block: at most [`POLL_SLICE`], never past `deadline`.
fn wait_for(deadline: Option<Instant>) -> Duration {
    deadline.map_or(POLL_SLICE, |d| POLL_SLICE.min(d.saturating_duration_since(Instant::now())))
}

/// The local queue and settle-once bookkeeping over one transport.
pub(crate) struct Core<T: Transport> {
    id: u64,
    pub(crate) transport: T,
    on_undecodable: Option<UndecodableHook>,
    state: Mutex<State<T::Native>>,
    healthy: AtomicBool,
}

impl<T: Transport> std::fmt::Debug for Core<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("id", &self.id)
            .field("healthy", &self.healthy.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<T: Transport> Core<T> {
    fn new(transport: T, on_undecodable: Option<UndecodableHook>) -> Self {
        Self {
            id: NEXT_BROKER.fetch_add(1, Ordering::Relaxed),
            transport,
            on_undecodable,
            state: Mutex::new(State::default()),
            healthy: AtomicBool::new(true),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<T::Native>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record whether a transport call worked. Only the adapter name is logged: client errors
    /// may embed credentials.
    fn record<V>(&self, result: Result<V, BrokerError>) -> Result<V, BrokerError> {
        let was_healthy = self.healthy.swap(result.is_ok(), Ordering::Relaxed);
        if result.is_err() && was_healthy {
            tracing::warn!("broker call failed; the {} adapter is unhealthy", T::NAME);
        } else if result.is_ok() && !was_healthy {
            tracing::info!("broker connection healthy again");
        }
        result
    }

    // -- publish and settle ---------------------------------------------------------------

    /// Run `fut` for at most the transport's I/O timeout plus `extra` (a wait the call was asked
    /// to make); a call still running then is abandoned and fails as `timed out`.
    async fn bounded<V>(
        &self,
        extra: Duration,
        fut: impl Future<Output = Result<V, BrokerError>>,
    ) -> Result<V, BrokerError> {
        let limit = self.transport.io_timeout().saturating_add(extra);
        match tokio::time::timeout(limit, fut).await {
            Ok(result) => result,
            Err(_) => Err(BrokerError::Other(format!("{}: timed out", T::NAME))),
        }
    }

    async fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        let sent = self.bounded(Duration::ZERO, self.transport.send(topic, envelope)).await;
        self.record(sent.map_err(into_publish))
    }

    /// Settle a delivery as processed (a delivery of another broker, or one already settled, is
    /// a no-op). A settle the transport failed leaves the delivery in flight, so the caller can
    /// retry it with the same [`Delivery`].
    async fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        let Some(held) = self.claim(delivery) else { return Ok(()) };
        let acked = self.bounded(Duration::ZERO, self.transport.ack(&held.native)).await;
        if acked.is_err() {
            self.restore(delivery.tag, held);
        }
        self.record(acked)
    }

    /// Requeue the delivery locally at the head of its topic with `attempt + 1` (saturating).
    /// The native message stays un-acked, so a crash still means redelivery by the broker.
    fn requeue(&self, delivery: &Delivery) {
        let Some(held) = self.claim(delivery) else { return };
        let again = held.envelope.with_attempt(held.envelope.attempt().saturating_add(1));
        self.state().local.entry(held.topic).or_default().push_front(Local {
            envelope: again,
            native: held.native,
            at: held.at,
        });
    }

    /// Settle the delivery without redelivery: dead-lettering is the dispatcher's job. A failed
    /// settle leaves the delivery in flight, so the caller can retry it.
    async fn drop_delivery(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        let Some(held) = self.claim(delivery) else { return Ok(()) };
        let dropped = self.bounded(Duration::ZERO, self.transport.drop_message(&held.native)).await;
        if dropped.is_err() {
            self.restore(delivery.tag, held);
        }
        self.record(dropped)
    }

    /// Close the client connection the transport opened.
    async fn close(&self) -> Result<(), BrokerError> {
        self.bounded(Duration::ZERO, self.transport.close()).await
    }

    fn claim(&self, delivery: &Delivery) -> Option<Inflight<T::Native>> {
        if delivery.broker_id != self.id {
            return None;
        }
        self.state().inflight.remove(&delivery.tag)
    }

    /// Put a claimed delivery back after a failed settle, so the caller can retry it.
    fn restore(&self, tag: u64, held: Inflight<T::Native>) {
        self.state().inflight.entry(tag).or_insert(held);
    }

    /// Start `op` on the native handle of a delivery that is handed out and not yet settled,
    /// without settling it (`None` for a delivery of another broker or one already settled).
    /// Used for broker-specific operations on a held message, such as SQS `extend_visibility`.
    #[cfg(feature = "sqs")]
    fn on_native<Fut>(
        self: &Arc<Self>,
        delivery: &Delivery,
        op: impl FnOnce(&T::Native) -> Fut,
    ) -> Option<HeldFuture>
    where
        Fut: Future<Output = Result<(), BrokerError>> + Send + 'static,
    {
        if delivery.broker_id != self.id {
            return None;
        }
        let fut = op(&self.state().inflight.get(&delivery.tag)?.native);
        let core = Arc::clone(self);
        Some(Box::pin(async move {
            let result = core.bounded(Duration::ZERO, fut).await;
            core.record(result)
        }))
    }

    // -- receive --------------------------------------------------------------------------

    /// The next message of the local queue as a delivery, without blocking. Entries held past
    /// half the redelivery window are released instead: forgotten locally without settling, so
    /// the broker redelivers them on its own clock rather than this process delivering them too
    /// late (which would inflate attempts and dead-letter healthy messages).
    fn pop(&self, topic: &str) -> Option<Delivery> {
        let hold = self.transport.redelivery_window() / 2;
        let mut released: Vec<T::Native> = Vec::new();
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

    fn hand_out(
        &self,
        st: &mut State<T::Native>,
        topic: &str,
        entry: Local<T::Native>,
    ) -> Delivery {
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

    /// Fetch from the broker (waiting up to `wait`) and put the decodable messages in the local
    /// queue; the undecodable ones are reported and dropped. Landing the messages in the queue
    /// instead of returning them means a caller that gives up mid-fetch loses nothing: the next
    /// `pop` delivers them.
    async fn fill(&self, topic: &str, wait: Duration) -> Result<(), BrokerError> {
        let fetched = self.bounded(wait, self.transport.fetch(topic, wait)).await;
        let raws = self.record(fetched)?;
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
        // after stashing: a failing drop loses none of the good ones
        for native in &bad {
            let dropped = self.bounded(Duration::ZERO, self.transport.drop_message(native)).await;
            self.record(dropped)?;
        }
        Ok(())
    }

    /// One fetched message as an envelope, or `None` (reported) when it cannot be decoded. The
    /// attempt is the broker's count only.
    fn decode(&self, topic: &str, raw: &Raw<T::Native>) -> Option<Envelope> {
        match Envelope::from_json_bytes(&raw.body, self.transport.max_bytes()) {
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

/// The Tokio runtime a broker's clients run on.
struct Runner {
    runtime: Option<Runtime>,
}

impl Runner {
    fn new() -> Result<Self, EdaError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(RUNTIME_THREADS)
            .thread_name("xsm-broker")
            .enable_all()
            .build()
            .map_err(|e| EdaError::Broker(BrokerError::Other(format!("tokio runtime: {e}"))))?;
        Ok(Self { runtime: Some(runtime) })
    }

    fn runtime(&self) -> Result<&Runtime, BrokerError> {
        self.runtime.as_ref().ok_or_else(|| BrokerError::Other("broker is shut down".to_owned()))
    }

    /// Start `fut` on the owned runtime. The returned future only awaits the join handle: it is
    /// runtime-agnostic, and dropping it does not cancel `fut`.
    fn spawn<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, BrokerError>> + Send + use<F>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.runtime().map(|rt| rt.spawn(fut));
        async move { handle?.await.map_err(|e| BrokerError::Other(format!("broker task failed: {e}"))) }
    }

    /// Run `fut` on the owned runtime and block this thread until it finishes, at most `limit`.
    /// Safe to call from inside another runtime too (it blocks that worker, it does not panic).
    /// A call that outlives `limit` is abandoned (it keeps running on the runtime, its result
    /// discarded) and fails as `timed out`.
    fn block<F>(&self, fut: F, limit: Duration) -> Result<F::Output, BrokerError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        drop(self.runtime()?.spawn(async move {
            // the receiver is gone once the caller timed out or panicked
            let _ = tx.send(fut.await);
        }));
        rx.recv_timeout(limit).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => BrokerError::Other("timed out".to_owned()),
            mpsc::RecvTimeoutError::Disconnected => {
                BrokerError::Other("broker task failed".to_owned())
            }
        })
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        // `Runtime::drop` panics inside an async context: the last handle may be dropped by a
        // task running on the runtime itself.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").finish_non_exhaustive()
    }
}

/// A broker over one transport: the [`Core`] plus the runtime its calls run on.
pub(crate) struct Broker<T: Transport> {
    core: Arc<Core<T>>,
    runner: Runner,
}

impl<T: Transport> std::fmt::Debug for Broker<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker").field("core", &self.core).finish_non_exhaustive()
    }
}

impl<T: Transport> Broker<T> {
    pub(crate) fn new(
        transport: T,
        on_undecodable: Option<UndecodableHook>,
    ) -> Result<Self, EdaError> {
        Ok(Self { core: Arc::new(Core::new(transport, on_undecodable)), runner: Runner::new()? })
    }

    pub(crate) fn healthy(&self) -> bool {
        self.core.healthy.load(Ordering::Relaxed)
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.core.state().inflight.len()
    }

    pub(crate) fn held(&self, topic: &str) -> usize {
        self.core.state().local.get(topic).map_or(0, VecDeque::len)
    }

    // -- async contract -------------------------------------------------------------------

    pub(crate) fn publish_async(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send + use<T> {
        let (core, topic, envelope) = (Arc::clone(&self.core), topic.to_owned(), envelope.clone());
        let run = self.runner.spawn(async move { core.publish(&topic, &envelope).await });
        async move { run.await? }
    }

    pub(crate) async fn poll_async(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        let deadline = deadline_after(timeout);
        let mut fetched = false;
        loop {
            if let Some(delivery) = self.core.pop(topic) {
                return Ok(Some(delivery));
            }
            if fetched && expired(deadline) {
                return Ok(None);
            }
            fetched = true;
            let (core, topic, wait) =
                (Arc::clone(&self.core), topic.to_owned(), wait_for(deadline));
            self.runner.spawn(async move { core.fill(&topic, wait).await }).await??;
        }
    }

    pub(crate) fn ack_async(
        &self,
        delivery: &Delivery,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send + use<T> {
        let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
        let run = self.runner.spawn(async move { core.ack(&delivery).await });
        async move { run.await? }
    }

    pub(crate) fn nack_async(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send + use<T> {
        let run = if requeue {
            // purely local: no round trip, no need for the runtime
            self.core.requeue(delivery);
            None
        } else {
            let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
            Some(self.runner.spawn(async move { core.drop_delivery(&delivery).await }))
        };
        async move {
            match run {
                Some(run) => run.await?,
                None => Ok(()),
            }
        }
    }

    pub(crate) fn close_async(
        &self,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send + use<T> {
        let core = Arc::clone(&self.core);
        let run = self.runner.spawn(async move { core.close().await });
        async move { run.await? }
    }

    /// Run `op` against the native handle of a held delivery on the owned runtime, without
    /// settling it; a no-op for a delivery of another broker or one already settled.
    #[cfg(feature = "sqs")]
    pub(crate) async fn on_native_async<Fut>(
        &self,
        delivery: &Delivery,
        op: impl FnOnce(&T::Native) -> Fut,
    ) -> Result<(), BrokerError>
    where
        Fut: Future<Output = Result<(), BrokerError>> + Send + 'static,
    {
        match self.core.on_native(delivery, op) {
            Some(fut) => self.runner.spawn(fut).await?,
            None => Ok(()),
        }
    }

    /// Blocking [`on_native_async`](Self::on_native_async).
    #[cfg(feature = "sqs")]
    pub(crate) fn on_native_blocking<Fut>(
        &self,
        delivery: &Delivery,
        op: impl FnOnce(&T::Native) -> Fut,
    ) -> Result<(), BrokerError>
    where
        Fut: Future<Output = Result<(), BrokerError>> + Send + 'static,
    {
        match self.core.on_native(delivery, op) {
            Some(fut) => self.runner.block(fut, self.block_limit(Duration::ZERO))?,
            None => Ok(()),
        }
    }

    // -- blocking contract ----------------------------------------------------------------

    /// How long a blocking call may take: the transport's I/O timeout, the wait it was asked to
    /// make and a grace so the transport's own timeout reports first.
    fn block_limit(&self, wait: Duration) -> Duration {
        self.core.transport.io_timeout().saturating_add(wait).saturating_add(BLOCK_GRACE)
    }

    pub(crate) fn publish_blocking(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> Result<(), BrokerError> {
        let (core, topic, envelope) = (Arc::clone(&self.core), topic.to_owned(), envelope.clone());
        self.runner.block(
            async move { core.publish(&topic, &envelope).await },
            self.block_limit(Duration::ZERO),
        )?
    }

    pub(crate) fn poll_blocking(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        let deadline = deadline_after(timeout);
        let mut fetched = false;
        loop {
            if let Some(delivery) = self.core.pop(topic) {
                return Ok(Some(delivery));
            }
            if fetched && expired(deadline) {
                return Ok(None);
            }
            fetched = true;
            let wait = wait_for(deadline);
            let (core, topic) = (Arc::clone(&self.core), topic.to_owned());
            let limit = self.block_limit(wait);
            self.runner.block(async move { core.fill(&topic, wait).await }, limit)??;
        }
    }

    pub(crate) fn ack_blocking(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
        self.runner
            .block(async move { core.ack(&delivery).await }, self.block_limit(Duration::ZERO))?
    }

    pub(crate) fn nack_blocking(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> Result<(), BrokerError> {
        if requeue {
            self.core.requeue(delivery);
            return Ok(());
        }
        let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
        self.runner.block(
            async move { core.drop_delivery(&delivery).await },
            self.block_limit(Duration::ZERO),
        )?
    }

    pub(crate) fn close_blocking(&self) -> Result<(), BrokerError> {
        let core = Arc::clone(&self.core);
        self.runner.block(async move { core.close().await }, self.block_limit(Duration::ZERO))?
    }
}

/// Implement [`BrokerAdapter`](crate::BrokerAdapter) and
/// [`SyncBrokerAdapter`](crate::SyncBrokerAdapter) for a newtype whose `inner` field is a
/// [`Broker`].
macro_rules! impl_broker_traits {
    ($broker:ty) => {
        impl $crate::broker::SyncBrokerAdapter for $broker {
            fn publish(
                &self,
                topic: &str,
                envelope: &$crate::envelope::Envelope,
            ) -> Result<(), $crate::error::BrokerError> {
                self.inner.publish_blocking(topic, envelope)
            }

            fn poll(
                &self,
                topic: &str,
                timeout: Option<std::time::Duration>,
            ) -> Result<Option<$crate::broker::Delivery>, $crate::error::BrokerError> {
                self.inner.poll_blocking(topic, timeout)
            }

            fn ack(
                &self,
                delivery: &$crate::broker::Delivery,
            ) -> Result<(), $crate::error::BrokerError> {
                self.inner.ack_blocking(delivery)
            }

            fn nack(
                &self,
                delivery: &$crate::broker::Delivery,
                requeue: bool,
            ) -> Result<(), $crate::error::BrokerError> {
                self.inner.nack_blocking(delivery, requeue)
            }
        }

        impl $crate::broker::BrokerAdapter for $broker {
            fn publish(
                &self,
                topic: &str,
                envelope: &$crate::envelope::Envelope,
            ) -> impl std::future::Future<Output = Result<(), $crate::error::BrokerError>> + Send
            {
                self.inner.publish_async(topic, envelope)
            }

            async fn poll(
                &self,
                topic: &str,
                timeout: Option<std::time::Duration>,
            ) -> Result<Option<$crate::broker::Delivery>, $crate::error::BrokerError> {
                self.inner.poll_async(topic, timeout).await
            }

            fn ack(
                &self,
                delivery: &$crate::broker::Delivery,
            ) -> impl std::future::Future<Output = Result<(), $crate::error::BrokerError>> + Send
            {
                self.inner.ack_async(delivery)
            }

            fn nack(
                &self,
                delivery: &$crate::broker::Delivery,
                requeue: bool,
            ) -> impl std::future::Future<Output = Result<(), $crate::error::BrokerError>> + Send
            {
                self.inner.nack_async(delivery, requeue)
            }
        }
    };
}

pub(crate) use impl_broker_traits;

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A transport whose first `ack` fails and whose second succeeds; `fetch` always offers one
    /// message.
    struct FailOnceAck {
        acks: AtomicUsize,
    }

    impl Transport for FailOnceAck {
        type Native = ();

        const NAME: &'static str = "fail-once";

        fn redelivery_window(&self) -> Duration {
            Duration::ZERO
        }

        fn max_bytes(&self) -> usize {
            1 << 20
        }

        fn io_timeout(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn send(
            &self,
            _topic: &str,
            _envelope: &Envelope,
        ) -> impl Future<Output = Result<(), BrokerError>> + Send {
            std::future::ready(Ok(()))
        }

        fn fetch(
            &self,
            _topic: &str,
            _wait: Duration,
        ) -> impl Future<Output = Result<Vec<Raw<()>>, BrokerError>> + Send {
            let envelope = Envelope::new("t").build().expect("envelope");
            let body = envelope.to_json(self.max_bytes()).expect("json").into_bytes();
            std::future::ready(Ok(vec![Raw { body, native: (), attempts: 0 }]))
        }

        fn ack(&self, _native: &()) -> impl Future<Output = Result<(), BrokerError>> + Send {
            let acked = if self.acks.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(BrokerError::Other("ack failed".into()))
            } else {
                Ok(())
            };
            std::future::ready(acked)
        }

        fn drop_message(
            &self,
            _native: &(),
        ) -> impl Future<Output = Result<(), BrokerError>> + Send {
            std::future::ready(Ok(()))
        }

        fn close(&self) -> impl Future<Output = Result<(), BrokerError>> + Send {
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn a_failed_ack_leaves_the_delivery_retriable() {
        let broker = Broker::new(FailOnceAck { acks: AtomicUsize::new(0) }, None).expect("broker");
        let delivery = broker
            .poll_blocking("topic", Some(Duration::from_secs(1)))
            .expect("poll")
            .expect("a delivery");
        assert_eq!(broker.in_flight(), 1);

        let err = broker.ack_blocking(&delivery).expect_err("the first ack fails");
        assert!(err.to_string().contains("ack failed"), "{err}");
        assert_eq!(broker.in_flight(), 1, "a failed ack must leave the delivery retriable");

        broker.ack_blocking(&delivery).expect("the retry succeeds");
        assert_eq!(broker.in_flight(), 0);
    }
}
