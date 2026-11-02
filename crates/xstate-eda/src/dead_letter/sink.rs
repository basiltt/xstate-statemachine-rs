//! Sinks: closures ([`sink_fn`]) and the broker sink publishing to `<topic>.dlq`.

use std::sync::Arc;

use xstate_core::redact::RedactKeys;

use super::{DeadLetter, DeadLetterSink, dlq_topic, redact_record};
use crate::broker::SyncBrokerAdapter;
use crate::envelope::Envelope;
use crate::error::Result;

impl<T: DeadLetterSink + ?Sized> DeadLetterSink for Arc<T> {
    fn put(&self, record: DeadLetter) -> Result<()> {
        (**self).put(record)
    }
}

/// A closure used as a [`DeadLetterSink`]; build with [`sink_fn`].
pub struct FnSink<F>(F);

impl<F> std::fmt::Debug for FnSink<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FnSink(..)")
    }
}

/// Wrap `f` as a sink (Python: any `callable(DeadLetter)`).
pub fn sink_fn<F>(f: F) -> FnSink<F>
where
    F: Fn(DeadLetter) -> Result<()> + Send + Sync,
{
    FnSink(f)
}

impl<F> DeadLetterSink for FnSink<F>
where
    F: Fn(DeadLetter) -> Result<()> + Send + Sync,
{
    fn put(&self, record: DeadLetter) -> Result<()> {
        (self.0)(record)
    }
}

/// Publish dead letters to `<topic>.dlq` on a broker (Python `BrokerDeadLetterSink`).
///
/// The published envelope has type `xsm.deadletter`, subject the original subject (else the
/// machine id), source `xsm/<machine id>` and the redacted record as `data`. Optionally the
/// record is first written to a `store`, so an operator can replay it.
///
/// Rust sinks are synchronous (the dispatcher and observers call them inline), so this sink
/// takes a [`SyncBrokerAdapter`]. For an async broker, build the envelope with
/// [`envelope_for`](Self::envelope_for) and publish it from your task.
pub struct BrokerDeadLetterSink<B> {
    broker: B,
    topic: String,
    store: Option<Arc<dyn DeadLetterSink>>,
}

impl<B> std::fmt::Debug for BrokerDeadLetterSink<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerDeadLetterSink").field("topic", &self.topic).finish_non_exhaustive()
    }
}

impl<B> BrokerDeadLetterSink<B> {
    /// A sink publishing to [`dlq_topic`]`(topic)`.
    pub fn new(broker: B, topic: &str) -> Self {
        Self { broker, topic: dlq_topic(topic), store: None }
    }

    /// Also write every record to `store` first.
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn DeadLetterSink>) -> Self {
        self.store = Some(store);
        self
    }

    /// The `.dlq` topic.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The broker.
    pub fn broker(&self) -> &B {
        &self.broker
    }

    /// Wrap a (redacted) dead letter in an `xsm.deadletter` envelope.
    ///
    /// # Errors
    /// The record does not form a valid envelope (data too deep, ...).
    pub fn envelope_for(record: &DeadLetter) -> Result<Envelope> {
        let rec = redact_record(record, &RedactKeys::default());
        let subject = rec
            .envelope
            .as_ref()
            .and_then(|e| e.get("subject"))
            .and_then(|s| match s {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) if s.is_empty() => None,
                serde_json::Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            })
            .unwrap_or_else(|| rec.machine_id.clone());
        Ok(Envelope::new("xsm.deadletter")
            .subject(subject)
            .source(format!("xsm/{}", rec.machine_id))
            .data(rec.to_dict())
            .machineid(rec.machine_id.clone())
            .machineversion(rec.machine_version.clone())
            .build()?)
    }
}

impl<B: SyncBrokerAdapter> DeadLetterSink for BrokerDeadLetterSink<B> {
    fn put(&self, record: DeadLetter) -> Result<()> {
        let env = Self::envelope_for(&record)?;
        if let Some(store) = &self.store {
            store.put(record)?;
        }
        self.broker.publish(&self.topic, &env)?;
        Ok(())
    }
}
