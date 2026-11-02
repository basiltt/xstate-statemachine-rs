//! The two broker traits over the shared `Core`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use super::RedisStreamsBroker;
use super::shared::{Core, deadline_after, expired, wait_for};
use crate::broker::{BrokerAdapter, Delivery, SyncBrokerAdapter};
use crate::envelope::Envelope;
use crate::error::BrokerError;

impl SyncBrokerAdapter for RedisStreamsBroker {
    fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        self.core.publish(topic, envelope)
    }

    fn poll(
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
            self.core.fill(topic, wait_for(deadline))?;
        }
    }

    fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        self.core.ack(delivery)
    }

    fn nack(&self, delivery: &Delivery, requeue: bool) -> Result<(), BrokerError> {
        self.core.nack(delivery, requeue)
    }
}

/// Run a blocking Redis call on Tokio's blocking pool.
async fn blocking<T: Send + 'static>(
    core: Arc<Core>,
    op: impl FnOnce(&Core) -> Result<T, BrokerError> + Send + 'static,
) -> Result<T, BrokerError> {
    tokio::task::spawn_blocking(move || op(&core))
        .await
        .map_err(|e| BrokerError::Other(format!("blocking task failed: {e}")))?
}

impl BrokerAdapter for RedisStreamsBroker {
    fn publish(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send {
        let (core, topic, envelope) = (Arc::clone(&self.core), topic.to_owned(), envelope.clone());
        blocking(core, move |c| c.publish(&topic, &envelope))
    }

    async fn poll(
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
            let topic = topic.to_owned();
            blocking(Arc::clone(&self.core), move |c| c.fill(&topic, wait)).await?;
        }
    }

    fn ack(&self, delivery: &Delivery) -> impl Future<Output = Result<(), BrokerError>> + Send {
        let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
        blocking(core, move |c| c.ack(&delivery))
    }

    fn nack(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send {
        let (core, delivery) = (Arc::clone(&self.core), delivery.clone());
        async move {
            if requeue {
                // purely local: no round trip, no need for a thread
                return core.nack(&delivery, true);
            }
            blocking(core, move |c| c.nack(&delivery, false)).await
        }
    }
}
