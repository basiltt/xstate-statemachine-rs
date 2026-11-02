//! Consumer loops: drain a topic through a sync or async broker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::{DispatchResult, EnvelopeHandler, InboundDispatcher};
use crate::broker::{BrokerAdapter, Delivery, SyncBrokerAdapter};
use crate::error::BrokerError;

/// How one delivery is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    Ack,
    Requeue,
}

/// A subject's deliveries, in arrival order.
type Group = Vec<Delivery>;
/// The outcome of a subject group: counters and how to settle each delivery (by index).
type Plan = (DispatchResult, Vec<(usize, Settle)>);

/// Group a batch by subject, keeping first-seen subject order (`None` groups as `"None"`).
fn by_subject(batch: Vec<Delivery>) -> Vec<Group> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for d in batch {
        let subject = d.envelope.subject.clone().unwrap_or_else(|| "None".into());
        let i = *index.entry(subject).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[i].push(d);
    }
    groups
}

/// Remember the first failure of a settle step; later ones are only logged.
fn note(first: &mut Option<BrokerError>, result: Result<(), BrokerError>) {
    if let Err(error) = result {
        if first.is_some() {
            tracing::warn!(
                target: "xstate::eda",
                error = error.type_name(),
                "another broker failure while settling a batch"
            );
        } else {
            *first = Some(error);
        }
    }
}

/// Hand back deliveries that were pulled but never settled, last first so the first ends up
/// at the head of its topic. Best effort: the broker is already failing, so a failure here is
/// logged (the broker redelivers un-acked messages on its own).
fn requeue_sync<'a, B: SyncBrokerAdapter>(
    broker: &B,
    deliveries: impl DoubleEndedIterator<Item = &'a Delivery>,
) {
    for d in deliveries.rev() {
        if let Err(error) = broker.nack(d, true) {
            tracing::warn!(target: "xstate::eda", error = error.type_name(), "could not requeue a pulled delivery");
        }
    }
}

/// [`requeue_sync`] for an async broker.
async fn requeue_async<'a, B: BrokerAdapter>(
    broker: &B,
    deliveries: impl DoubleEndedIterator<Item = &'a Delivery>,
) {
    for d in deliveries.rev() {
        if let Err(error) = broker.nack(d, true).await {
            tracing::warn!(target: "xstate::eda", error = error.type_name(), "could not requeue a pulled delivery");
        }
    }
}

impl<H: EnvelopeHandler> InboundDispatcher<H> {
    /// Pull up to `max_batch` deliveries; a failure requeues what was already pulled.
    fn pull_sync<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        timeout: Duration,
    ) -> Result<Vec<Delivery>, BrokerError> {
        let mut batch = Vec::new();
        for next in broker.subscribe(topic, Some(timeout)) {
            match next {
                Ok(d) => batch.push(d),
                Err(error) => {
                    requeue_sync(broker, batch.iter());
                    return Err(error);
                }
            }
            if batch.len() >= self.max_batch {
                break;
            }
        }
        Ok(batch)
    }

    /// Handle a subject's deliveries one by one and settle each at once; a retry requeues it
    /// and the later ones. A settle failure is noted and the rest of the group still settles.
    fn settle_group_sync<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        group: &[Delivery],
        total: &mut DispatchResult,
        first_error: &mut Option<BrokerError>,
    ) {
        for (n, d) in group.iter().enumerate() {
            let res = self.handle(&d.envelope, Some(topic));
            let retried = res.retried > 0;
            total.merge(res);
            if retried {
                for later in group[n..].iter().rev() {
                    note(first_error, broker.nack(later, true));
                }
                return;
            }
            note(first_error, broker.ack(d));
        }
    }

    /// Handle a subject's deliveries in order; on a retry, requeue it and every later one of
    /// the subject, last first, so the head is the failed delivery.
    fn plan_group(&self, topic: &str, group: &[Delivery]) -> Plan {
        let mut total = DispatchResult::default();
        let mut settles = Vec::with_capacity(group.len());
        for (n, d) in group.iter().enumerate() {
            let res = self.handle(&d.envelope, Some(topic));
            let retried = res.retried > 0;
            total.merge(res);
            if retried {
                settles.extend((n..group.len()).rev().map(|i| (i, Settle::Requeue)));
                return (total, settles);
            }
            settles.push((n, Settle::Ack));
        }
        (total, settles)
    }

    /// Plan every group, up to `max_in_flight` subjects at once on scoped worker threads.
    fn plan_groups(&self, topic: &str, groups: &[Group]) -> Vec<Plan> {
        let workers = self.max_in_flight.min(groups.len());
        if workers <= 1 {
            return groups.iter().map(|g| self.plan_group(topic, g)).collect();
        }
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Plan>>> = groups.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(group) = groups.get(i) else { break };
                        let plan = self.plan_group(topic, group);
                        *slots[i].lock().unwrap_or_else(PoisonError::into_inner) = Some(plan);
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|m| m.into_inner().unwrap_or_else(PoisonError::into_inner).unwrap_or_default())
            .collect()
    }

    /// Drain what `topic` holds now through a [`SyncBrokerAdapter`], waiting up to `timeout`
    /// of idle time; subjects are processed one after another, deliveries settled at once.
    ///
    /// A broker failure while pulling requeues what was pulled before it. A failure while
    /// settling is returned only after the rest of the batch has been handled and settled.
    ///
    /// # Errors
    /// The first broker failure while polling or settling.
    pub fn run_once_sync<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        timeout: Duration,
    ) -> Result<DispatchResult, BrokerError> {
        let batch = self.pull_sync(broker, topic, timeout)?;
        let mut total = DispatchResult::default();
        let mut first_error = None;
        for group in by_subject(batch) {
            self.settle_group_sync(broker, topic, &group, &mut total, &mut first_error);
        }
        first_error.map_or(Ok(total), Err)
    }

    /// Drain what `topic` holds now through an async [`BrokerAdapter`] (waiting up to
    /// `timeout` of idle time for deliveries).
    ///
    /// Up to `max_in_flight` subjects are handled concurrently on scoped worker threads (the
    /// handler is synchronous); the calling task blocks while the batch's handlers run, so on
    /// a multi-threaded runtime call it from a blocking-capable context (e.g. tokio's
    /// `block_in_place`) when handlers are slow. Deliveries are acked / nacked afterwards.
    ///
    /// A broker failure while pulling or settling requeues the deliveries of the batch that
    /// are still unsettled before the error is returned.
    ///
    /// # Errors
    /// A broker failure while polling or settling.
    pub async fn run_once<B: BrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        timeout: Duration,
    ) -> Result<DispatchResult, BrokerError> {
        let mut batch = Vec::new();
        while batch.len() < self.max_batch {
            match broker.poll(topic, Some(timeout)).await {
                Ok(Some(d)) => batch.push(d),
                Ok(None) => break,
                Err(error) => {
                    requeue_async(broker, batch.iter()).await;
                    return Err(error);
                }
            }
        }
        let groups = by_subject(batch);
        let plans = self.plan_groups(topic, &groups);
        let mut total = DispatchResult::default();
        for (group, (result, settles)) in groups.iter().zip(plans) {
            total.merge(result);
            for (i, settle) in settles {
                let settled = match settle {
                    Settle::Ack => broker.ack(&group[i]).await,
                    Settle::Requeue => broker.nack(&group[i], true).await,
                };
                if let Err(error) = settled {
                    // settling twice is a no-op: only the unsettled ones go back
                    requeue_async(broker, groups.iter().flatten()).await;
                    return Err(error);
                }
            }
        }
        Ok(total)
    }

    /// [`run_once`](Self::run_once) in a loop until `stop` is set.
    ///
    /// # Errors
    /// The first broker failure.
    pub async fn run_forever<B: BrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        stop: &AtomicBool,
        poll_timeout: Duration,
    ) -> Result<DispatchResult, BrokerError> {
        let mut total = DispatchResult::default();
        while !stop.load(Ordering::Acquire) {
            total.merge(self.run_once(broker, topic, poll_timeout).await?);
        }
        Ok(total)
    }

    /// [`run_once_sync`](Self::run_once_sync) in a loop until `stop` is set.
    ///
    /// # Errors
    /// The first broker failure.
    pub fn run_forever_sync<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        topic: &str,
        stop: &AtomicBool,
        poll_timeout: Duration,
    ) -> Result<DispatchResult, BrokerError> {
        let mut total = DispatchResult::default();
        while !stop.load(Ordering::Acquire) {
            total.merge(self.run_once_sync(broker, topic, poll_timeout)?);
        }
        Ok(total)
    }
}
