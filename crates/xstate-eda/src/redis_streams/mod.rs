//! Redis Streams broker adapter (feature `redis-streams`): [`RedisStreamsBroker`], the async
//! [`BrokerAdapter`](crate::BrokerAdapter) and the blocking
//! [`SyncBrokerAdapter`](crate::SyncBrokerAdapter) over consumer groups.
//!
//! A port of Python's `contrib/brokers/redis_streams.py` that writes the same stream layout and
//! entry fields, so a Rust and a Python service can publish to and consume from one stream:
//!
//! ```text
//! {prefix}:stream:{topic}          the stream (shards = 1)
//! {prefix}:stream:{topic}:{n}      shard n of `shards`
//! entry fields:  ce       structured-mode CloudEvents JSON (the size cap applies)
//!                subject  the envelope subject, or ""
//! ```
//!
//! * **Partition key** = the envelope's `subject`: with `shards > 1` a subject always hashes
//!   (`crc32`, identical in Python) to one shard, so per-subject order holds.
//! * **Consumer group** (created lazily with `MKSTREAM`, from id `0`, so nothing published before
//!   the first consumer is lost) plus a consumer name. `ack` is `XACK`; `nack(requeue = false)`
//!   is `XACK` too, because dead-lettering is the dispatcher's job; `nack(requeue = true)` puts
//!   the delivery back at the head of a local queue with `attempt + 1` while the entry stays
//!   pending (a crash still means redelivery).
//! * **Crash recovery**: entries a dead consumer left pending for at least `min_idle_ms` are
//!   claimed (`XPENDING` + `XCLAIM`) before new entries are read; their delivery count becomes
//!   the envelope's attempt, so a poison message is dead-lettered across restarts. A message
//!   held in the local queue for more than half of `min_idle_ms` is released so the broker's
//!   clock, not this process, redelivers it.
//! * `maxlen` trims the stream approximately on publish; `None` keeps everything.
//!
//! The one type implements both traits over blocking connections. The async methods run the
//! Redis calls on Tokio's blocking pool, so they must be called inside a Tokio runtime; the sync
//! ones need none. A `poll` that is cancelled mid-fetch loses nothing: fetched messages land in
//! the local queue and the next `poll` delivers them.

use std::sync::Arc;

use crate::envelope::DEFAULT_MAX_SNAPSHOT_BYTES;
use crate::error::{EdaError, EnvelopeError, Result};

mod adapters;
mod pool;
mod shared;
mod transport;

use self::pool::Pool;
use self::shared::Core;
use self::transport::{Settings, Transport};

/// Called with the topic and the reason for every inbound message that could not be decoded
/// (the message is acked and dropped; its body is never logged).
pub type UndecodableHook = Arc<dyn Fn(&str, &EnvelopeError) + Send + Sync>;

/// Default consumer group.
pub const DEFAULT_GROUP: &str = "xsm";
/// Default idle time after which another consumer's pending entry may be reclaimed (60 s).
pub const DEFAULT_MIN_IDLE_MS: u64 = 60_000;
/// Default entries fetched per read: small, because fetched entries wait locally while the
/// broker's redelivery clock runs.
pub const DEFAULT_BATCH: usize = 10;

/// Options of a [`RedisStreamsBroker`].
#[derive(Clone)]
pub struct RedisStreamsOptions {
    /// The key namespace: non-empty, no whitespace; trailing colons are stripped.
    pub prefix: String,
    /// Consumer group shared by the workers of one service.
    pub group: String,
    /// This consumer's name; `None` = `hostname-pid`.
    pub consumer: Option<String>,
    /// Streams per topic; subjects hash to one shard. At least 1.
    pub shards: usize,
    /// Reclaim another consumer's pending entries after this idle time (crash recovery).
    pub min_idle_ms: u64,
    /// Approximate stream cap on publish (`XADD MAXLEN ~`); `None` = unbounded.
    pub maxlen: Option<u64>,
    /// Entries fetched per read, at least 1.
    pub batch: usize,
    /// Size cap of an envelope on the wire, checked on publish and before parsing on receive.
    pub max_bytes: usize,
    /// Called for every undecodable inbound message.
    pub on_undecodable: Option<UndecodableHook>,
}

impl std::fmt::Debug for RedisStreamsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStreamsOptions")
            .field("prefix", &self.prefix)
            .field("group", &self.group)
            .field("consumer", &self.consumer)
            .field("shards", &self.shards)
            .field("min_idle_ms", &self.min_idle_ms)
            .field("maxlen", &self.maxlen)
            .field("batch", &self.batch)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl RedisStreamsOptions {
    /// Defaults under `prefix`: group `xsm`, one shard, 60 s reclaim, batches of 10, unbounded
    /// streams and the 1 MiB envelope cap.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            group: DEFAULT_GROUP.to_owned(),
            consumer: None,
            shards: 1,
            min_idle_ms: DEFAULT_MIN_IDLE_MS,
            maxlen: None,
            batch: DEFAULT_BATCH,
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            on_undecodable: None,
        }
    }

    /// Use the consumer group `group`.
    #[must_use]
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }

    /// Name this consumer.
    #[must_use]
    pub fn consumer(mut self, consumer: impl Into<String>) -> Self {
        self.consumer = Some(consumer.into());
        self
    }

    /// Spread every topic over `shards` streams.
    #[must_use]
    pub fn shards(mut self, shards: usize) -> Self {
        self.shards = shards;
        self
    }

    /// Reclaim pending entries idle for `min_idle_ms`.
    #[must_use]
    pub fn min_idle_ms(mut self, min_idle_ms: u64) -> Self {
        self.min_idle_ms = min_idle_ms;
        self
    }

    /// Trim streams to about `maxlen` entries on publish.
    #[must_use]
    pub fn maxlen(mut self, maxlen: u64) -> Self {
        self.maxlen = Some(maxlen);
        self
    }

    /// Fetch `batch` entries per read.
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
        hook: impl Fn(&str, &EnvelopeError) + Send + Sync + 'static,
    ) -> Self {
        self.on_undecodable = Some(Arc::new(hook));
        self
    }

    fn settings(&self) -> Result<Settings> {
        let invalid = |why: &str| Err(EdaError::InvalidArgument(why.to_owned()));
        if self.prefix.trim().is_empty() {
            return invalid("Redis backends need a non-empty `prefix` (the key namespace)");
        }
        if self.prefix.contains([' ', '\t', '\n', '\r']) {
            return invalid("Redis `prefix` must not contain whitespace");
        }
        let prefix = self.prefix.trim_end_matches(':');
        if prefix.is_empty() {
            return invalid("Redis `prefix` must not consist of colons only");
        }
        if self.group.is_empty() {
            return invalid("the consumer group must not be empty");
        }
        if self.shards < 1 {
            return invalid("shards must be >= 1");
        }
        if self.batch < 1 {
            return invalid("batch must be >= 1");
        }
        if self.maxlen == Some(0) {
            return invalid("maxlen must be >= 1");
        }
        Ok(Settings {
            prefix: prefix.to_owned(),
            group: self.group.clone(),
            consumer: self.consumer.clone().unwrap_or_else(default_consumer),
            shards: self.shards,
            min_idle_ms: self.min_idle_ms,
            maxlen: self.maxlen,
            batch: self.batch,
            max_bytes: self.max_bytes,
        })
    }
}

/// `hostname-pid` (Python `_default_consumer`).
fn default_consumer() -> String {
    let host = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "localhost".to_owned());
    format!("{host}-{}", std::process::id())
}

/// A broker over Redis Streams consumer groups, implementing both
/// [`BrokerAdapter`](crate::BrokerAdapter) (async, on Tokio's blocking pool) and
/// [`SyncBrokerAdapter`](crate::SyncBrokerAdapter). See the [module documentation](self).
///
/// The two traits share method names: with both in scope call through the trait you mean
/// (`SyncBrokerAdapter::poll(&broker, ..)`). Deliveries can be settled through either trait.
#[derive(Debug, Clone)]
pub struct RedisStreamsBroker {
    core: Arc<Core>,
}

impl RedisStreamsBroker {
    /// Connect to `url` (`redis://host:6379/0`, `rediss://` for TLS). Redis is contacted on first
    /// use.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for an invalid URL or option.
    pub fn open(url: &str, options: &RedisStreamsOptions) -> Result<Self> {
        let settings = options.settings()?;
        let pool = Pool::open(url).map_err(EdaError::InvalidArgument)?;
        let transport = Transport::new(pool, settings);
        Ok(Self { core: Arc::new(Core::new(transport, options.on_undecodable.clone())) })
    }

    /// `false` after a Redis call failed, until one succeeds.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.core.healthy()
    }

    /// Deliveries handed out and not yet settled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.core.in_flight()
    }

    /// Messages fetched (or requeued) locally on `topic` and not yet delivered.
    #[must_use]
    pub fn held(&self, topic: &str) -> usize {
        self.core.held(topic)
    }
}
