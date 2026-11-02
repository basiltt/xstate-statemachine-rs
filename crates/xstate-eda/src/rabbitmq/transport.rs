//! The four native operations over one AMQP channel: `send`, `fetch` (`basic.get`), `ack` and
//! `reject`, plus the queue naming.
//!
//! Python reference: `contrib/brokers/rabbitmq.py` (`RabbitMQTransport`).
//!
//! ```text
//! queue       {topic}                durable; published through the default exchange with
//!                                    routing key = topic (persistent, publisher confirms)
//! exchange    {exchange}             when configured: routing key = envelope subject, and the
//!                                    queues are bound by the operator (consistent-hash recipe)
//! properties  content-type  application/cloudevents+json; charset=UTF-8
//!             message-id    the envelope id
//!             type          the envelope type
//!             delivery-mode 2 (persistent)
//! headers     ce_subject    the envelope subject, or ""
//! ```

use std::collections::HashSet;
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lapin::acker::Acker;
use lapin::options::{
    BasicAckOptions, BasicGetOptions, BasicPublishOptions, BasicQosOptions, BasicRejectOptions,
    ConfirmSelectOptions, ExchangeDeclareOptions, QueueDeclareOptions,
};
use lapin::publisher_confirm::Confirmation;
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Channel, Connection, ExchangeKind};

use super::ConnectFactory;
use crate::async_core::{CE_CONTENT_TYPE, Raw, Transport};
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// What settles one delivery.
pub(super) type Native = Acker;

/// Pause of a fetch that found the queue empty (`basic.get` answers "empty" at once; waiting is
/// a sleep, and the poll loop above retries).
const EMPTY_POLL: Duration = Duration::from_millis(50);

/// AMQP short strings (message id, type) are at most 255 bytes.
const SHORT_STRING_MAX: usize = 255;

/// `delivery-mode` of a persistent message.
const PERSISTENT: u8 = 2;

/// AMQP close code of a normal shutdown.
const REPLY_SUCCESS: u16 = 200;

/// The transport's settings (the broker options after validation).
#[derive(Debug, Clone)]
pub(super) struct Settings {
    pub(super) exchange: Option<String>,
    pub(super) prefetch: usize,
    pub(super) batch: usize,
    pub(super) max_bytes: usize,
}

/// Where the channel comes from.
pub(super) enum Source {
    /// Connect lazily (and reconnect after a failure) to this URL. It may carry a password: it
    /// is never logged.
    Url { url: String, connect_with: Option<ConnectFactory> },
    /// A channel the caller manages (Python `channel=`): never closed by the adapter.
    Channel,
}

#[derive(Default)]
struct Cache {
    connection: Option<Arc<Connection>>,
    channel: Option<Channel>,
    /// Queues declared on the current channel.
    queues: HashSet<String>,
    /// The configured exchange exists (checked on the current channel).
    exchange_ready: bool,
}

/// AMQP operations over one channel.
pub(super) struct RabbitTransport {
    source: Source,
    pub(super) settings: Settings,
    cache: Mutex<Cache>,
    /// Messages fetched and not yet acked or rejected. `basic.qos` only bounds `basic.consume`,
    /// not `basic.get`, so the adapter enforces `prefetch` itself.
    unacked: AtomicUsize,
}

fn failed(error: impl Display) -> BrokerError {
    BrokerError::Other(format!("rabbitmq: {error}"))
}

/// A failure while preparing a publish is a publish failure.
fn into_publish(error: BrokerError) -> BrokerError {
    match error {
        BrokerError::Other(why) => BrokerError::Publish(why),
        other => other,
    }
}

fn short_string(what: &str, value: &str) -> Result<ShortString, BrokerError> {
    if value.len() > SHORT_STRING_MAX {
        return Err(BrokerError::Publish(format!(
            "rabbitmq: the envelope {what} is longer than {SHORT_STRING_MAX} bytes"
        )));
    }
    Ok(ShortString::from(value))
}

/// Deliveries the broker made before this one: quorum queues stamp `x-delivery-count`; classic
/// queues only say `redelivered` (counted as 1).
fn attempts_of(properties: &BasicProperties, redelivered: bool) -> u64 {
    let count = properties
        .headers()
        .as_ref()
        .and_then(|table| table.inner().get("x-delivery-count"))
        .and_then(|value| match value {
            AMQPValue::ShortShortInt(n) => u64::try_from(*n).ok(),
            AMQPValue::ShortShortUInt(n) => Some(u64::from(*n)),
            AMQPValue::ShortInt(n) => u64::try_from(*n).ok(),
            AMQPValue::ShortUInt(n) => Some(u64::from(*n)),
            AMQPValue::LongInt(n) => u64::try_from(*n).ok(),
            AMQPValue::LongUInt(n) => Some(u64::from(*n)),
            AMQPValue::LongLongInt(n) => u64::try_from(*n).ok(),
            _ => None,
        });
    count.unwrap_or_else(|| u64::from(redelivered))
}

impl RabbitTransport {
    pub(super) fn new(source: Source, settings: Settings, channel: Option<Channel>) -> Self {
        let cache = Cache { channel, ..Cache::default() };
        Self { source, settings, cache: Mutex::new(cache), unacked: AtomicUsize::new(0) }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connected channel: the cached one, else a new channel on the live connection, else a new
    /// connection (after a failure).
    async fn channel(&self) -> Result<Channel, BrokerError> {
        let (channel, connection) = {
            let cache = self.cache();
            (cache.channel.clone(), cache.connection.clone())
        };
        if let Some(channel) = channel
            && channel.status().connected()
        {
            return Ok(channel);
        }
        let Source::Url { url, connect_with } = &self.source else {
            return Err(failed("the channel is closed"));
        };
        let connection = match connection {
            Some(live) if live.status().connected() => live,
            _ => {
                let properties = connect_with.as_ref().map_or_else(Default::default, |f| f());
                Arc::new(Connection::connect(url, properties).await.map_err(failed)?)
            }
        };
        let channel = self.open_channel(&connection).await?;
        *self.cache() = Cache {
            connection: Some(connection),
            channel: Some(channel.clone()),
            ..Cache::default()
        };
        // messages fetched on the dead channel can no longer be settled: the broker redelivers
        self.unacked.store(0, Ordering::Relaxed);
        Ok(channel)
    }

    /// A channel with publisher confirms and the `QoS` window.
    async fn open_channel(&self, connection: &Connection) -> Result<Channel, BrokerError> {
        let channel = connection.create_channel().await.map_err(failed)?;
        channel.confirm_select(ConfirmSelectOptions::default()).await.map_err(failed)?;
        let prefetch = u16::try_from(self.settings.prefetch).unwrap_or(u16::MAX);
        channel.basic_qos(prefetch, BasicQosOptions::default()).await.map_err(failed)?;
        Ok(channel)
    }

    /// Declare the durable queue of `topic` once per channel (a publish to no queue is lost).
    async fn queue(&self, channel: &Channel, topic: &str) -> Result<(), BrokerError> {
        if self.cache().queues.contains(topic) {
            return Ok(());
        }
        let options = QueueDeclareOptions { durable: true, ..QueueDeclareOptions::default() };
        channel.queue_declare(topic, options, FieldTable::default()).await.map_err(failed)?;
        self.cache().queues.insert(topic.to_owned());
        Ok(())
    }

    /// The configured exchange must exist (a passive declare, once per channel).
    async fn check_exchange(&self, channel: &Channel, exchange: &str) -> Result<(), BrokerError> {
        if self.cache().exchange_ready {
            return Ok(());
        }
        let options = ExchangeDeclareOptions { passive: true, ..ExchangeDeclareOptions::default() };
        channel
            .exchange_declare(exchange, ExchangeKind::Direct, options, FieldTable::default())
            .await
            .map_err(failed)?;
        self.cache().exchange_ready = true;
        Ok(())
    }

    fn release(&self) {
        // saturate at zero: a reconnect resets the count while old deliveries may still settle
        let mut current = self.unacked.load(Ordering::Relaxed);
        while current > 0 {
            match self.unacked.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn properties(envelope: &Envelope) -> Result<BasicProperties, BrokerError> {
        let mut headers = FieldTable::default();
        let subject = envelope.subject.as_deref().unwrap_or_default();
        headers.insert(
            ShortString::from("ce_subject"),
            AMQPValue::LongString(LongString::from(subject)),
        );
        Ok(BasicProperties::default()
            .with_content_type(ShortString::from(CE_CONTENT_TYPE))
            .with_message_id(short_string("id", &envelope.id)?)
            .with_type(short_string("type", &envelope.ty)?)
            .with_delivery_mode(PERSISTENT)
            .with_headers(headers))
    }

    async fn publish(
        &self,
        topic: &str,
        envelope: &Envelope,
        body: &[u8],
    ) -> Result<(), BrokerError> {
        let properties = Self::properties(envelope)?;
        let channel = self.channel().await?;
        let (exchange, routing_key) = match &self.settings.exchange {
            None => {
                self.queue(&channel, topic).await?;
                ("", topic)
            }
            Some(exchange) => {
                self.check_exchange(&channel, exchange).await?;
                (exchange.as_str(), envelope.subject.as_deref().unwrap_or_default())
            }
        };
        let confirm = channel
            .basic_publish(exchange, routing_key, BasicPublishOptions::default(), body, properties)
            .await
            .map_err(failed)?
            .await
            .map_err(failed)?;
        match confirm {
            Confirmation::Nack(_) => Err(failed("the broker refused the message (nack)")),
            Confirmation::Ack(_) | Confirmation::NotRequested => Ok(()),
        }
    }
}

impl Transport for RabbitTransport {
    type Native = Native;

    const NAME: &'static str = "RabbitMQ";

    /// AMQP has no visibility timeout: an un-acked message is redelivered when its channel
    /// closes, so locally held messages are never released.
    fn redelivery_window(&self) -> Duration {
        Duration::ZERO
    }

    fn max_bytes(&self) -> usize {
        self.settings.max_bytes
    }

    /// Publish persistently with publisher confirms.
    async fn send(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        let body = envelope.to_json(self.settings.max_bytes)?;
        self.publish(topic, envelope, body.as_bytes()).await.map_err(into_publish)
    }

    /// `basic.get` up to `min(batch, prefetch - unacked)` messages.
    async fn fetch(&self, topic: &str, wait: Duration) -> Result<Vec<Raw<Native>>, BrokerError> {
        let channel = self.channel().await?;
        self.queue(&channel, topic).await?;
        let room = self
            .settings
            .batch
            .min(self.settings.prefetch.saturating_sub(self.unacked.load(Ordering::Relaxed)));
        let mut out = Vec::new();
        while out.len() < room {
            let Some(message) = channel
                .basic_get(topic, BasicGetOptions { no_ack: false })
                .await
                .map_err(failed)?
            else {
                break;
            };
            let delivery = message.delivery;
            out.push(Raw {
                attempts: attempts_of(&delivery.properties, delivery.redelivered),
                native: delivery.acker,
                body: delivery.data,
            });
            self.unacked.fetch_add(1, Ordering::Relaxed);
        }
        if out.is_empty() && !wait.is_zero() {
            tokio::time::sleep(wait.min(EMPTY_POLL)).await;
        }
        Ok(out)
    }

    async fn ack(&self, native: &Native) -> Result<(), BrokerError> {
        native.ack(BasicAckOptions::default()).await.map_err(failed)?;
        self.release();
        Ok(())
    }

    /// `basic.reject` without requeue: a broker-side dead-letter exchange, if configured, sees
    /// the reject.
    async fn drop_message(&self, native: &Native) -> Result<(), BrokerError> {
        native.reject(BasicRejectOptions { requeue: false }).await.map_err(failed)?;
        self.release();
        Ok(())
    }

    /// Close a connection the adapter opened (a channel the caller manages is left alone).
    async fn close(&self) -> Result<(), BrokerError> {
        // The channel handle must outlive `connection.close`: dropping it first starts a
        // channel close of its own and the connection close then fails on a closing channel.
        let (connection, channel) = {
            let mut cache = self.cache();
            match cache.connection.take() {
                Some(connection) => {
                    let channel = cache.channel.take();
                    *cache = Cache::default();
                    (Some(connection), channel)
                }
                // a channel the caller manages stays usable
                None => (None, None),
            }
        };
        self.unacked.store(0, Ordering::Relaxed);
        let result = match connection {
            Some(connection) => connection.close(REPLY_SUCCESS, "bye").await.map_err(failed),
            None => Ok(()),
        };
        drop(channel);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_count(value: AMQPValue) -> BasicProperties {
        let mut headers = FieldTable::default();
        headers.insert(ShortString::from("x-delivery-count"), value);
        BasicProperties::default().with_headers(headers)
    }

    #[test]
    fn attempts_follow_the_delivery_count_then_the_redelivered_flag() {
        let plain = BasicProperties::default();
        assert_eq!(attempts_of(&plain, false), 0);
        assert_eq!(attempts_of(&plain, true), 1);
        assert_eq!(attempts_of(&with_count(AMQPValue::LongLongInt(4)), false), 4);
        assert_eq!(attempts_of(&with_count(AMQPValue::LongInt(2)), true), 2);
        assert_eq!(attempts_of(&with_count(AMQPValue::LongInt(-1)), true), 1, "negative: ignored");
        assert_eq!(attempts_of(&with_count(AMQPValue::Boolean(true)), false), 0);
    }

    #[test]
    fn long_short_strings_are_refused() {
        assert!(short_string("type", &"x".repeat(255)).is_ok());
        assert!(matches!(short_string("type", &"x".repeat(256)), Err(BrokerError::Publish(_))));
    }
}
