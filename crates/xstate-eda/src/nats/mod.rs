//! NATS `JetStream` broker adapter (feature `nats`): [`NatsBroker`], the async
//! [`BrokerAdapter`](crate::BrokerAdapter) and the blocking
//! [`SyncBrokerAdapter`](crate::SyncBrokerAdapter) over durable pull consumers.
//!
//! A port of Python's `contrib/brokers/nats.py` that uses the same streams, subjects, headers
//! and consumer names, so a Rust and a Python service can publish to and consume from one topic.
//! Core NATS is at-most-once; the adapter uses `JetStream` (persisted, acked) only:
//!
//! ```text
//! stream    {topic}                    captures {topic}.>
//! subject   {topic}.{subject token}    the envelope subject with `.`, `*`, `>` and whitespace
//!                                      replaced by `_` (empty: `_`)
//! consumer  {durable}                  one durable pull consumer per (topic, durable) reads the
//!                                      whole stream in order, so per-subject order is the
//!                                      stream's order
//! headers   Nats-Msg-Id = envelope id  JetStream's duplicate window drops a re-published
//!                                      outbox row server-side
//!           content-type = application/cloudevents+json; charset=UTF-8
//! body      structured-mode CloudEvents JSON (the size cap applies)
//! ```
//!
//! * **Consumer group** = the durable consumer name: every worker of one service uses the same
//!   `durable` and `JetStream` shares the stream between them; another service uses another name
//!   and gets its own copy.
//! * `ack` is `+ACK`; `nack(requeue = false)` is `+TERM` (never redelivered, dead-lettering is
//!   the dispatcher's job); `nack(requeue = true)` puts the delivery back at the head of a local
//!   queue with `attempt + 1` while the message stays un-acked (a crash still means redelivery).
//! * The envelope's attempt is the server's delivery count (`num_delivered - 1`); the wire
//!   `xsmattempt` extension is never trusted. `ack_wait` is `JetStream`'s redelivery timeout: a
//!   message held longer is redelivered, so one held in the local queue for more than half of
//!   `ack_wait` is released to the server's clock instead of being delivered late.
//! * Undecodable or oversized messages are reported to the `on_undecodable` hook and `+TERM`ed.
//!
//! The one type implements both traits. The adapter owns a small Tokio runtime that the NATS
//! client runs on, so neither trait needs the caller to be inside a runtime; the blocking
//! methods may be called from plain threads. A `poll` that is cancelled mid-fetch loses nothing:
//! fetched messages land in the local queue and the next `poll` delivers them. The connection is
//! made lazily, on first use.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_nats::{ConnectOptions, ServerAddr};

pub use crate::async_core::UndecodableHook;
use crate::async_core::{Broker, impl_broker_traits};
use crate::envelope::DEFAULT_MAX_SNAPSHOT_BYTES;
use crate::error::{BrokerError, EdaError, Result};

mod transport;

pub use self::transport::subject_token;
use self::transport::{NatsTransport, Settings, Source};

/// Default durable consumer name.
pub const DEFAULT_DURABLE: &str = "xsm";
/// Default `JetStream` redelivery timeout for un-acked messages (30 s).
pub const DEFAULT_ACK_WAIT: Duration = Duration::from_secs(30);
/// Default messages fetched per request: small, because fetched messages wait locally while the
/// server's redelivery clock runs.
pub const DEFAULT_BATCH: usize = 10;

/// Builds the options of a (re)connection: TLS, credentials, name. Called on every connect, so
/// secrets stay in the closure and are never echoed.
pub(crate) type ConnectFactory = Arc<dyn Fn() -> ConnectOptions + Send + Sync>;

/// Options of a [`NatsBroker`].
#[derive(Clone)]
pub struct NatsOptions {
    /// Durable consumer name: one per consumer service, shared by its workers.
    pub durable: String,
    /// `JetStream` redelivery timeout for un-acked messages.
    pub ack_wait: Duration,
    /// Messages fetched per request, at least 1.
    pub batch: usize,
    /// Size cap of an envelope on the wire, checked on publish and before parsing on receive.
    pub max_bytes: usize,
    /// Called for every undecodable inbound message.
    pub on_undecodable: Option<UndecodableHook>,
    connect_with: Option<ConnectFactory>,
}

impl std::fmt::Debug for NatsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsOptions")
            .field("durable", &self.durable)
            .field("ack_wait", &self.ack_wait)
            .field("batch", &self.batch)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for NatsOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl NatsOptions {
    /// Defaults: durable `xsm`, 30 s ack wait, batches of 10 and the 1 MiB envelope cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            durable: DEFAULT_DURABLE.to_owned(),
            ack_wait: DEFAULT_ACK_WAIT,
            batch: DEFAULT_BATCH,
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            on_undecodable: None,
            connect_with: None,
        }
    }

    /// Use the durable consumer `durable`.
    #[must_use]
    pub fn durable(mut self, durable: impl Into<String>) -> Self {
        self.durable = durable.into();
        self
    }

    /// Redeliver un-acked messages after `ack_wait`.
    #[must_use]
    pub fn ack_wait(mut self, ack_wait: Duration) -> Self {
        self.ack_wait = ack_wait;
        self
    }

    /// Fetch `batch` messages per request.
    #[must_use]
    pub fn batch(mut self, batch: usize) -> Self {
        self.batch = batch;
        self
    }

    /// Refuse envelopes larger than `max_bytes` on the wire.
    #[must_use]
    pub fn max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Be told about every undecodable inbound message.
    #[must_use]
    pub fn on_undecodable(
        mut self,
        hook: impl Fn(&str, &crate::EnvelopeError) + Send + Sync + 'static,
    ) -> Self {
        self.on_undecodable = Some(Arc::new(hook));
        self
    }

    /// Connect with the options `factory` builds (TLS, credentials; Python `connect_kw`). It runs
    /// on every (re)connect, and what it returns is never logged.
    #[must_use]
    pub fn connect_with(
        mut self,
        factory: impl Fn() -> ConnectOptions + Send + Sync + 'static,
    ) -> Self {
        self.connect_with = Some(Arc::new(factory));
        self
    }

    fn settings(&self) -> Result<Settings> {
        let invalid = |why: &str| Err(EdaError::InvalidArgument(why.to_owned()));
        let unsafe_name = |c: char| c.is_whitespace() || matches!(c, '.' | '*' | '>' | '/' | '\\');
        if self.durable.is_empty() || self.durable.contains(unsafe_name) {
            return invalid(
                "the durable consumer name must be non-empty and free of whitespace, `.`, `*`, \
                 `>` and path separators",
            );
        }
        if self.ack_wait.is_zero() {
            return invalid("ack_wait must be > 0");
        }
        if self.batch < 1 {
            return invalid("batch must be >= 1");
        }
        Ok(Settings {
            durable: self.durable.clone(),
            ack_wait: self.ack_wait,
            batch: self.batch,
            max_bytes: self.max_bytes,
        })
    }
}

/// A broker over NATS `JetStream`, implementing both [`BrokerAdapter`](crate::BrokerAdapter) (async)
/// and [`SyncBrokerAdapter`](crate::SyncBrokerAdapter). See the [module documentation](self).
///
/// The two traits share method names: with both in scope call through the trait you mean
/// (`SyncBrokerAdapter::poll(&broker, ..)`). Deliveries can be settled through either trait.
/// Clones share one connection and one local queue.
#[derive(Debug, Clone)]
pub struct NatsBroker {
    inner: Arc<Broker<NatsTransport>>,
}

impl NatsBroker {
    /// Connect to `servers` (`nats://host:4222`; several URLs separated by commas). NATS is
    /// contacted on first use.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid URL or option.
    pub fn open(servers: &str, options: &NatsOptions) -> Result<Self> {
        let invalid = || EdaError::InvalidArgument("invalid NATS URL".to_owned());
        let addrs = servers
            .split(',')
            .map(|url| match url.trim() {
                "" => Err(invalid()),
                url => ServerAddr::from_str(url).map_err(|_| invalid()),
            })
            .collect::<Result<Vec<_>>>()?;
        let source = Source::Servers { addrs, connect_with: options.connect_with.clone() };
        Self::build(source, None, options)
    }

    /// Use a `JetStream` context you manage (Python `js=`); the adapter never closes its
    /// connection. Its tasks must outlive this broker's use: the context belongs to the runtime
    /// that created it.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid option.
    pub fn with_jetstream(
        context: async_nats::jetstream::Context,
        options: &NatsOptions,
    ) -> Result<Self> {
        Self::build(Source::Context, Some(context), options)
    }

    fn build(
        source: Source,
        context: Option<async_nats::jetstream::Context>,
        options: &NatsOptions,
    ) -> Result<Self> {
        let transport = NatsTransport::new(source, options.settings()?, context);
        let inner = Broker::new(transport, options.on_undecodable.clone())?;
        Ok(Self { inner: Arc::new(inner) })
    }

    /// `false` after a NATS call failed, until one succeeds.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.inner.healthy()
    }

    /// Deliveries handed out and not yet settled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight()
    }

    /// Messages fetched (or requeued) locally on `topic` and not yet delivered.
    #[must_use]
    pub fn held(&self, topic: &str) -> usize {
        self.inner.held(topic)
    }

    /// Drain and close the connection this broker opened (not a context passed to
    /// [`with_jetstream`](Self::with_jetstream)). A later call reconnects.
    ///
    /// # Errors
    /// The drain failed.
    pub async fn close(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_async().await
    }

    /// Blocking [`close`](Self::close).
    ///
    /// # Errors
    /// The drain failed.
    pub fn close_blocking(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_blocking()
    }
}

impl_broker_traits!(NatsBroker);
