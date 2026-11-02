//! Amazon SQS broker adapter (feature `sqs`): [`SqsBroker`], the async
//! [`BrokerAdapter`](crate::BrokerAdapter) and the blocking
//! [`SyncBrokerAdapter`](crate::SyncBrokerAdapter) over a queue per topic.
//!
//! A port of Python's `contrib/brokers/sqs.py` that uses the same queues, bodies and attributes,
//! so a Rust and a Python service can publish to and consume from one topic:
//!
//! ```text
//! queue      {topic}                  the queue NAME, resolved once with GetQueueUrl; the adapter
//!                                     never creates queues
//! body       structured-mode CloudEvents JSON (the size cap applies; SQS itself caps at 256 KiB)
//! attribute  ce_type = envelope type
//! FIFO       topics ending in ".fifo": MessageGroupId = envelope subject (or "_"; per-subject
//!            order is SQS's per-group order) and MessageDeduplicationId = envelope id (a
//!            re-published outbox row inside SQS's 5-minute window is dropped server-side)
//! ```
//!
//! STANDARD queues give no ordering at all: use them only where the chart tolerates reordering.
//!
//! * `ack` and `nack(requeue = false)` are `DeleteMessage` (dead-lettering is the dispatcher's
//!   job; an SQS redrive policy is a second net for consumers that crash in a loop).
//!   `nack(requeue = true)` puts the delivery back at the head of a local queue with
//!   `attempt + 1` while the message stays in flight (a crash still means redelivery).
//! * **Long polling**: whole seconds of the poll budget (at most 20) become `WaitTimeSeconds`;
//!   up to 10 messages (`batch`) are received at a time.
//! * The **visibility timeout** is SQS's redelivery clock: a message held longer is redelivered
//!   to someone else (at-least-once; dedup). Size it above your processing time or call
//!   [`SqsBroker::extend_visibility`]. A message held in the local queue for more than half of
//!   it is released to SQS's clock instead of being delivered late. Without an explicit
//!   [`visibility_timeout`](SqsOptions::visibility_timeout) the queue's own is used (the local
//!   hold window assumes SQS's default of 30 s).
//! * The envelope's attempt is `ApproximateReceiveCount - 1`; the wire `xsmattempt` extension is
//!   never trusted.
//! * Undecodable or oversized messages are reported to the `on_undecodable` hook and deleted.
//!
//! The client is the AWS SDK's: credentials, region and endpoint come from its environment-based
//! configuration unless [`SqsOptions`] overrides them (a local emulator needs an
//! [`endpoint_url`](SqsOptions::endpoint_url), a [`region`](SqsOptions::region) and
//! [`credentials`](SqsOptions::credentials)), or you pass a ready client to
//! [`SqsBroker::with_client`]. Credentials and the endpoint are never logged or echoed.
//!
//! The one type implements both traits. The adapter owns a small Tokio runtime that the SDK runs
//! on, so neither trait needs the caller to be inside a runtime; the blocking methods may be
//! called from plain threads. A `poll` that is cancelled mid-fetch loses nothing: fetched
//! messages land in the local queue and the next `poll` delivers them. The client is built
//! lazily, on first use.

use std::sync::Arc;
use std::time::Duration;

pub use crate::async_core::UndecodableHook;
use crate::async_core::{Broker, impl_broker_traits};
use crate::broker::Delivery;
use crate::envelope::DEFAULT_MAX_SNAPSHOT_BYTES;
use crate::error::{BrokerError, EdaError, Result};

mod transport;

use self::transport::{ClientConfig, MAX_BATCH, Settings, Source, SqsTransport, StaticCredentials};

/// Default messages received per request (the SQS maximum): keep it near your concurrency, as
/// received messages wait locally while the visibility clock runs.
pub const DEFAULT_BATCH: usize = MAX_BATCH;
/// The longest visibility timeout SQS accepts (12 hours).
pub const MAX_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(43_200);
/// Default bound on one broker call (build the client, resolve the queue, send, receive, settle).
pub const DEFAULT_IO_TIMEOUT: Duration = crate::async_core::DEFAULT_IO_TIMEOUT;

/// Options of a [`SqsBroker`].
#[derive(Clone)]
pub struct SqsOptions {
    /// Per-receive visibility timeout, truncated to whole seconds; also the local hold window.
    /// `None` = the queue's own (assumed 30 s, SQS's default, for the hold window).
    pub visibility_timeout: Option<Duration>,
    /// Messages received per request, 1 to 10 (larger values are capped at 10).
    pub batch: usize,
    /// Size cap of an envelope on the wire, checked on publish and before parsing on receive.
    pub max_bytes: usize,
    /// Bound on one broker call (building the client, resolving the queue, sending, receiving,
    /// settling); a receive may additionally wait as long as the poll asked. A call that
    /// exceeds it fails as `timed out`.
    pub io_timeout: Duration,
    /// Called for every undecodable inbound message.
    pub on_undecodable: Option<UndecodableHook>,
    endpoint_url: Option<String>,
    region: Option<String>,
    credentials: Option<StaticCredentials>,
}

impl std::fmt::Debug for SqsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the endpoint and the credentials are never printed, not even their length
        f.debug_struct("SqsOptions")
            .field("visibility_timeout", &self.visibility_timeout)
            .field("batch", &self.batch)
            .field("max_bytes", &self.max_bytes)
            .field("io_timeout", &self.io_timeout)
            .field("region", &self.region)
            .field("endpoint_url", &self.endpoint_url.as_ref().map(|_| "<set>"))
            .field("credentials", &self.credentials.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl Default for SqsOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl SqsOptions {
    /// Defaults: the queue's visibility timeout, batches of 10, the 1 MiB envelope cap and the
    /// SDK's environment-based endpoint, region and credentials.
    #[must_use]
    pub fn new() -> Self {
        Self {
            visibility_timeout: None,
            batch: DEFAULT_BATCH,
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            io_timeout: DEFAULT_IO_TIMEOUT,
            on_undecodable: None,
            endpoint_url: None,
            region: None,
            credentials: None,
        }
    }

    /// Hold received messages invisible for `timeout` (whole seconds, at most 12 hours).
    #[must_use]
    pub fn visibility_timeout(mut self, timeout: Duration) -> Self {
        self.visibility_timeout = Some(timeout);
        self
    }

    /// Receive `batch` messages per request.
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

    /// Bound every broker call by `io_timeout` (default 30 s).
    #[must_use]
    pub fn io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
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

    /// Talk to `url` instead of the AWS endpoint of the region (a local emulator such as
    /// `ElasticMQ`). Never logged.
    #[must_use]
    pub fn endpoint_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint_url = Some(url.into());
        self
    }

    /// Use the AWS region `region` instead of the environment's.
    #[must_use]
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Sign with these static credentials instead of the SDK's credential chain. Never logged.
    #[must_use]
    pub fn credentials(
        mut self,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        self.credentials = Some(StaticCredentials {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        });
        self
    }

    /// Add a session token to the static [`credentials`](Self::credentials) (no effect without
    /// them).
    #[must_use]
    pub fn session_token(mut self, token: impl Into<String>) -> Self {
        if let Some(credentials) = &mut self.credentials {
            credentials.session_token = Some(token.into());
        }
        self
    }

    fn settings(&self) -> Result<Settings> {
        let invalid = |why: &str| Err(EdaError::InvalidArgument(why.to_owned()));
        if self.batch < 1 {
            return invalid("batch must be >= 1");
        }
        if self.io_timeout.is_zero() {
            return invalid("io_timeout must be > 0");
        }
        if self.visibility_timeout.is_some_and(|t| t > MAX_VISIBILITY_TIMEOUT) {
            return invalid("visibility_timeout must be at most 12 hours");
        }
        if self.region.as_deref().is_some_and(str::is_empty) {
            return invalid("region must not be empty");
        }
        let well_formed = |url: &String| url.starts_with("http://") || url.starts_with("https://");
        if self.endpoint_url.as_ref().is_some_and(|url| !well_formed(url)) {
            return invalid("the endpoint URL must start with http:// or https://");
        }
        Ok(Settings {
            visibility_secs: self.visibility_timeout.map(visibility_secs),
            batch: self.batch,
            max_bytes: self.max_bytes,
            io_timeout: self.io_timeout,
        })
    }

    fn client_config(&self) -> ClientConfig {
        ClientConfig {
            endpoint_url: self.endpoint_url.clone(),
            region: self.region.clone(),
            credentials: self.credentials.clone(),
        }
    }
}

/// `timeout` as the whole seconds SQS takes (clamped to its 12-hour maximum).
fn visibility_secs(timeout: Duration) -> i32 {
    i32::try_from(timeout.min(MAX_VISIBILITY_TIMEOUT).as_secs()).unwrap_or(i32::MAX)
}

/// A broker over Amazon SQS, implementing both [`BrokerAdapter`](crate::BrokerAdapter) (async)
/// and [`SyncBrokerAdapter`](crate::SyncBrokerAdapter). See the [module documentation](self).
///
/// The two traits share method names: with both in scope call through the trait you mean
/// (`SyncBrokerAdapter::poll(&broker, ..)`). Deliveries can be settled through either trait.
/// Clones share one client and one local queue.
#[derive(Debug, Clone)]
pub struct SqsBroker {
    inner: Arc<Broker<SqsTransport>>,
}

impl SqsBroker {
    /// A broker over the client `options` describe (the SDK's environment-based configuration
    /// plus the overrides). Nothing is contacted until first use.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid option.
    pub fn open(options: &SqsOptions) -> Result<Self> {
        Self::build(Source::Config(options.client_config()), None, options)
    }

    /// Use an SQS client you built (Python `client=`): its endpoint, region and credentials win
    /// over those of `options`, and the adapter never drops it. The client should be used by
    /// this broker only: the connections it pools belong to the broker's own runtime.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid option.
    pub fn with_client(client: aws_sdk_sqs::Client, options: &SqsOptions) -> Result<Self> {
        Self::build(Source::Client, Some(client), options)
    }

    fn build(
        source: Source,
        client: Option<aws_sdk_sqs::Client>,
        options: &SqsOptions,
    ) -> Result<Self> {
        let transport = SqsTransport::new(source, options.settings()?, client);
        let inner = Broker::new(transport, options.on_undecodable.clone())?;
        Ok(Self { inner: Arc::new(inner) })
    }

    /// `false` after an SQS call failed, until one succeeds.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.inner.healthy()
    }

    /// Deliveries handed out and not yet settled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight()
    }

    /// Messages received (or requeued) locally on `topic` and not yet delivered.
    #[must_use]
    pub fn held(&self, topic: &str) -> usize {
        self.inner.held(topic)
    }

    /// Keep an in-flight delivery invisible for `by` more, from now (whole seconds, at most 12
    /// hours; SQS `ChangeMessageVisibility`). A delivery that is settled, or another broker's,
    /// is a no-op.
    ///
    /// # Errors
    /// SQS refused the change (for example the receipt handle expired).
    pub async fn extend_visibility(
        &self,
        delivery: &Delivery,
        by: Duration,
    ) -> std::result::Result<(), BrokerError> {
        let secs = visibility_secs(by);
        self.inner
            .on_native_async(delivery, |native| {
                let native = native.clone();
                async move { native.extend(secs).await }
            })
            .await
    }

    /// Blocking [`extend_visibility`](Self::extend_visibility).
    ///
    /// # Errors
    /// SQS refused the change.
    pub fn extend_visibility_blocking(
        &self,
        delivery: &Delivery,
        by: Duration,
    ) -> std::result::Result<(), BrokerError> {
        let secs = visibility_secs(by);
        self.inner.on_native_blocking(delivery, |native| {
            let native = native.clone();
            async move { native.extend(secs).await }
        })
    }

    /// Drop the client this broker built and its queue-URL cache (a client passed to
    /// [`with_client`](Self::with_client) is left alone). A later call reconnects.
    ///
    /// # Errors
    /// Never in practice; the signature matches the other adapters.
    pub async fn close(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_async().await
    }

    /// Blocking [`close`](Self::close).
    ///
    /// # Errors
    /// Never in practice.
    pub fn close_blocking(&self) -> std::result::Result<(), BrokerError> {
        self.inner.close_blocking()
    }
}

impl_broker_traits!(SqsBroker);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_endpoint_or_the_credentials() {
        let options = SqsOptions::new()
            .endpoint_url("http://secret-host:9324")
            .credentials("AKIDSECRET", "sekrit")
            .session_token("tok")
            .region("eu-west-1");
        let shown = format!("{options:?}");
        for secret in ["secret-host", "AKIDSECRET", "sekrit", "tok\"", "9324"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("eu-west-1"));
    }

    #[test]
    fn options_are_validated() {
        let bad = |o: SqsOptions| matches!(SqsBroker::open(&o), Err(EdaError::InvalidArgument(_)));
        assert!(bad(SqsOptions::new().batch(0)));
        assert!(bad(SqsOptions::new().region("")));
        assert!(bad(SqsOptions::new().endpoint_url("localhost:9324")));
        assert!(bad(SqsOptions::new().visibility_timeout(Duration::from_secs(43_201))));
        assert!(SqsBroker::open(&SqsOptions::new().batch(99)).is_ok());
    }

    #[test]
    fn visibility_is_truncated_to_whole_seconds_and_capped() {
        assert_eq!(visibility_secs(Duration::from_millis(1900)), 1);
        assert_eq!(visibility_secs(Duration::from_secs(1_000_000)), 43_200);
    }
}
