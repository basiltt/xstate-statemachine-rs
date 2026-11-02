//! The broker contract every adapter meets (Python `eda/broker.py`).
//!
//! * `publish(topic, envelope)`;
//! * `poll(topic, timeout)` returns the next [`Delivery`] (or `None` once the topic stayed idle
//!   for `timeout`); [`SyncBrokerAdapter::subscribe`] wraps it as an iterator, Python's
//!   `subscribe`;
//! * `ack` settles a delivery, `nack(requeue)` returns it to the **head** of the topic (so it is
//!   redelivered before anything published after it) or drops it. Settling twice is a no-op.
//!
//! Ordering is per `subject` (the partition key), never global. An un-acked delivery is not
//! redelivered until it is nacked: there is no visibility timeout in the contract. A consumer
//! that nacks one delivery must also nack the later deliveries of the same subject it already
//! holds ([`InboundDispatcher`](crate::InboundDispatcher) does) or per-subject order is lost.
//!
//! The async contract uses return-position `impl Future + Send` (no `async-trait`); an
//! implementation may write its methods as `async fn`. It is runtime-agnostic.

use std::future::Future;
use std::time::Duration;

use crate::envelope::Envelope;
use crate::error::BrokerError;

/// One received envelope plus the handle that settles it.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// The decoded envelope (a private copy: mutating it does not change a redelivery).
    pub envelope: Envelope,
    /// The topic it came from.
    pub topic: String,
    /// Broker-assigned delivery tag, unique per delivery.
    pub tag: u64,
    /// Identity of the broker instance that issued the delivery; another broker treats it as
    /// unknown (settling it is a no-op).
    pub broker_id: u64,
}

/// Async broker contract.
pub trait BrokerAdapter: Send + Sync {
    /// Publish `envelope` on `topic`.
    fn publish(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send;

    /// The next delivery from `topic` in per-subject order, waiting up to `timeout`
    /// (`None` = forever, `Some(ZERO)` = only what is ready). `Ok(None)` when nothing arrived.
    fn poll(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> impl Future<Output = Result<Option<Delivery>, BrokerError>> + Send;

    /// Settle a delivery as processed.
    fn ack(&self, delivery: &Delivery) -> impl Future<Output = Result<(), BrokerError>> + Send;

    /// Return a delivery to the head of its topic (`requeue`) or drop it.
    fn nack(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send;
}

/// Blocking broker contract (threads, workers, scripts without an async runtime).
pub trait SyncBrokerAdapter: Send + Sync {
    /// Publish `envelope` on `topic`.
    ///
    /// # Errors
    /// The broker refused the envelope.
    fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError>;

    /// The next delivery from `topic`, blocking up to `timeout` (`None` = forever).
    ///
    /// # Errors
    /// Broker failure.
    fn poll(&self, topic: &str, timeout: Option<Duration>)
    -> Result<Option<Delivery>, BrokerError>;

    /// Settle a delivery as processed.
    ///
    /// # Errors
    /// Broker failure.
    fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError>;

    /// Return a delivery to the head of its topic (`requeue`) or drop it.
    ///
    /// # Errors
    /// Broker failure.
    fn nack(&self, delivery: &Delivery, requeue: bool) -> Result<(), BrokerError>;

    /// Deliveries from `topic` until it stays idle for `timeout` (Python `subscribe`):
    /// `timeout` is *idle* time, so a backlog is never cut short; `None` blocks forever when
    /// idle. A delivery left unsettled when iteration stops stays in flight.
    fn subscribe<'a>(&'a self, topic: &'a str, timeout: Option<Duration>) -> Subscription<'a, Self>
    where
        Self: Sized,
    {
        Subscription { broker: self, topic, timeout, done: false }
    }
}

/// Iterator returned by [`SyncBrokerAdapter::subscribe`].
#[derive(Debug)]
pub struct Subscription<'a, B: SyncBrokerAdapter> {
    broker: &'a B,
    topic: &'a str,
    timeout: Option<Duration>,
    done: bool,
}

impl<B: SyncBrokerAdapter> Iterator for Subscription<'_, B> {
    type Item = Result<Delivery, BrokerError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.broker.poll(self.topic, self.timeout) {
            Ok(Some(d)) => Some(Ok(d)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
