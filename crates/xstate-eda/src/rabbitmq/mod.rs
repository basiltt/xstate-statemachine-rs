//! RabbitMQ broker adapter (feature `rabbitmq`): [`RabbitMqBroker`], the async
//! [`BrokerAdapter`](crate::BrokerAdapter) and the blocking
//! [`SyncBrokerAdapter`](crate::SyncBrokerAdapter) over AMQP 0-9-1 queues.
//!
//! A port of Python's `contrib/brokers/rabbitmq.py` (aio-pika) that publishes the same messages
//! to the same queues, so a Rust and a Python service can publish to and consume from one topic.
//! The thinnest mapping that keeps the broker contract:
//!
//! ```text
//! queue       {topic}              durable; the topic and the queue share a name
//! publish     default exchange, routing key = topic, persistent (delivery-mode 2), publisher
//!             confirms; the queue is declared first (a publish to no queue is lost)
//! body        structured-mode CloudEvents JSON (the size cap applies)
//! properties  content-type = application/cloudevents+json; charset=UTF-8,
//!             message-id = envelope id, type = envelope type
//! headers     ce_subject = the envelope subject, or ""
//! ```
//!
//! * **Consumer group** = the queue: workers that consume one queue share its messages;
//!   scaling out while keeping per-subject order is the consistent-hash exchange recipe (one
//!   queue per consumer, routing key = subject): set [`RabbitMqOptions::exchange`] and bind the
//!   queues yourself, and the adapter publishes through that exchange with `routing_key =
//!   subject`.
//! * `ack` is `basic.ack`; `nack(requeue = false)` is `basic.reject` without requeue
//!   (dead-lettering is the dispatcher's job, or configure a broker-side dead-letter exchange,
//!   which then sees the reject); `nack(requeue = true)` puts the delivery back at the head of a
//!   local queue with `attempt + 1` while the message stays un-acked (a crash or a closed
//!   channel means redelivery by the broker).
//! * The envelope's attempt is the broker's count: quorum queues stamp `x-delivery-count`;
//!   classic queues only say `redelivered`, counted as 1. The wire `xsmattempt` extension is
//!   never trusted.
//! * `prefetch` bounds the un-acked messages of the adapter (held locally plus handed out):
//!   backpressure. Messages are fetched with `basic.get`, which `basic.qos` does not bound, so
//!   the adapter enforces the limit itself.
//! * Undecodable or oversized messages are reported to the `on_undecodable` hook and rejected
//!   without requeue.
//! * TLS: an `amqps://` URL; further client options through [`RabbitMqOptions::connect_with`].
//!   `Debug` output never shows the URL.
//!
//! The one type implements both traits. The adapter owns a small Tokio runtime, so neither trait
//! needs the caller to be inside a runtime; the blocking methods may be called from plain
//! threads. A `poll` that is cancelled mid-fetch loses nothing: fetched messages land in the
//! local queue and the next `poll` delivers them. The connection is made lazily, on first use,
//! and re-made after a failure.

use std::sync::Arc;

use lapin::ConnectionProperties;
use lapin::uri::AMQPUri;

pub use crate::async_core::UndecodableHook;
use crate::async_core::{Broker, impl_broker_traits};
use crate::envelope::DEFAULT_MAX_SNAPSHOT_BYTES;
use crate::error::{BrokerError, EdaError, Result};

mod transport;

use self::transport::{RabbitTransport, Settings, Source};

/// Default un-acked messages per adapter (the `QoS` window).
pub const DEFAULT_PREFETCH: usize = 20;
/// Default messages fetched per read.
pub const DEFAULT_BATCH: usize = 10;

/// Builds the connection properties of a (re)connection (name, client properties). Called on
/// every connect.
pub(crate) type ConnectFactory = Arc<dyn Fn() -> ConnectionProperties + Send + Sync>;

/// Options of a [`RabbitMqBroker`].
#[derive(Clone)]
pub struct RabbitMqOptions {
    /// Publish through this exchange with `routing_key = subject` (consistent-hash recipe)
    /// instead of the default exchange.
    pub exchange: Option<String>,
    /// Un-acked messages in flight, at least 1.
    pub prefetch: usize,
    /// Messages fetched per read, at least 1.
    pub batch: usize,
    /// Size cap of an envelope on the wire, checked on publish and before parsing on receive.
    pub max_bytes: usize,
    /// Called for every undecodable inbound message.
    pub on_undecodable: Option<UndecodableHook>,
    connect_with: Option<ConnectFactory>,
}

impl std::fmt::Debug for RabbitMqOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RabbitMqOptions")
            .field("exchange", &self.exchange)
            .field("prefetch", &self.prefetch)
            .field("batch", &self.batch)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for RabbitMqOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl RabbitMqOptions {
    /// Defaults: the default exchange, prefetch 20, batches of 10 and the 1 MiB envelope cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            exchange: None,
            prefetch: DEFAULT_PREFETCH,
            batch: DEFAULT_BATCH,
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            on_undecodable: None,
            connect_with: None,
        }
    }

    /// Publish through `exchange` with the envelope subject as routing key.
    #[must_use]
    pub fn exchange(mut self, exchange: impl Into<String>) -> Self {
        self.exchange = Some(exchange.into());
        self
    }

    /// Allow `prefetch` un-acked messages.
    #[must_use]
    pub fn prefetch(mut self, prefetch: usize) -> Self {
        self.prefetch = prefetch;
        self
    }

    /// Fetch `batch` messages per read.
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

    /// Connect with the properties `factory` builds (Python `connect_kw`). It runs on every
    /// (re)connect.
    #[must_use]
    pub fn connect_with(
        mut self,
        factory: impl Fn() -> ConnectionProperties + Send + Sync + 'static,
    ) -> Self {
        self.connect_with = Some(Arc::new(factory));
        self
    }

    fn settings(&self) -> Result<Settings> {
        let invalid = |why: &str| Err(EdaError::InvalidArgument(why.to_owned()));
        if self.prefetch < 1 {
            return invalid("prefetch must be >= 1");
        }
        if self.batch < 1 {
            return invalid("batch must be >= 1");
        }
        if self.exchange.as_deref().is_some_and(str::is_empty) {
            return invalid("the exchange name must not be empty (leave it unset for the default)");
        }
        Ok(Settings {
            exchange: self.exchange.clone(),
            prefetch: self.prefetch,
            batch: self.batch,
            max_bytes: self.max_bytes,
        })
    }
}

/// A broker over RabbitMQ, implementing both [`BrokerAdapter`](crate::BrokerAdapter) (async) and
/// [`SyncBrokerAdapter`](crate::SyncBrokerAdapter). See the [module documentation](self).
///
/// The two traits share method names: with both in scope call through the trait you mean
/// (`SyncBrokerAdapter::poll(&broker, ..)`). Deliveries can be settled through either trait.
/// Clones share one connection and one local queue.
#[derive(Debug, Clone)]
pub struct RabbitMqBroker {
    inner: Arc<Broker<RabbitTransport>>,
}

impl RabbitMqBroker {
    /// Connect to `url` (`amqp://user:pw@host:5672/%2f`, `amqps://` for TLS). RabbitMQ is
    /// contacted on first use.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid URL or option.
    pub fn open(url: &str, options: &RabbitMqOptions) -> Result<Self> {
        // the URL may carry a password: neither it nor the parser's message is echoed
        if url.parse::<AMQPUri>().is_err() {
            return Err(EdaError::InvalidArgument("invalid AMQP URL".to_owned()));
        }
        let source =
            Source::Url { url: url.to_owned(), connect_with: options.connect_with.clone() };
        Self::build(source, None, options)
    }

    /// Use an open channel you manage (Python `channel=`): the adapter never closes it and does
    /// not reconnect. The channel belongs to the runtime that created it.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid option.
    pub fn with_channel(channel: lapin::Channel, options: &RabbitMqOptions) -> Result<Self> {
        Self::build(Source::Channel, Some(channel), options)
    }

    fn build(
        source: Source,
        channel: Option<lapin::Channel>,
        options: &RabbitMqOptions,
    ) -> Result<Self> {
        let transport = RabbitTransport::new(source, options.settings()?, channel);
        let inner = Broker::new(transport, options.on_undecodable.clone())?;
        Ok(Self { inner: Arc::new(inner) })
    }

    /// `false` after a RabbitMQ call failed, until one succeeds.
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

    /// Close the connection this broker opened (not a channel passed to
    /// [`with_channel`](Self::with_channel)). Un-acked messages are redelivered by the broker. A
    /// later call reconnects.
    ///
    /// # Errors
    /// The close failed.
    pub async fn close(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_async().await
    }

    /// Blocking [`close`](Self::close).
    ///
    /// # Errors
    /// The close failed.
    pub fn close_blocking(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_blocking()
    }
}

impl_broker_traits!(RabbitMqBroker);
