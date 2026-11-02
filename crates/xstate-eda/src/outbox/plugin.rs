//! [`OutboxPlugin`]: turn chart-declared transitions and `publish`-tagged states into outbound
//! envelopes (Python `OutboxPlugin`).

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};
use xstate_core::redact::RedactKeys;
use xstate_core::{
    ActiveState, Context, Event, Machine, Observer, StateId, StepReport, TransitionId,
};

use super::specs::{state_type, transition_spec};
use super::{OutboxStore, PUBLISH_TAG};
use crate::chart::{active_sorted, entered_per_transition, has_tag};
use crate::envelope::{Envelope, TransitionOrigin};
use crate::error::Result;

/// Collects the envelopes a machine's chart declares, as an [`Observer`].
///
/// * a transition with `meta.publish` yields an envelope of the declared type (`data` = the
///   listed context fields, or absent);
/// * entering a state tagged `publish` (including at start) yields
///   `xsm.<machine>.transition.<state key>` with the whole context, redacted, as `data`.
///
/// Envelopes carry `causationid` / `correlationid` / `traceparent` from the inbound cause set
/// with [`set_cause`](Self::set_cause) (the dispatcher's envelope), and the subject defaults
/// to the cause's subject, then [`instance_key`](Self::instance_key), then the machine id.
///
/// # Delivering the envelopes
///
/// Rust observers cannot reach the `persisted()` [`Session`](xstate_persistence::Session), so
/// the plugin buffers and the caller delivers, inside the `persisted` body after sending:
///
/// ```no_run
/// # use std::sync::Arc;
/// # use xstate_core::{Machine, MachineLogic, Observer};
/// # use xstate_eda::{OutboxPlugin, OutboxStore, MemoryOutboxStore, SyncBrokerAdapter, SyncFakeBrokerAdapter};
/// # use xstate_persistence::{MemoryStore, PersistedOptions, persisted};
/// # fn demo(machine: Machine, store: MemoryStore) -> xstate_persistence::Result<()> {
/// let plugin = Arc::new(OutboxPlugin::new().instance_key("order-1"));
/// let options = PersistedOptions { observers: vec![plugin.clone() as Arc<dyn Observer<_>>], ..Default::default() };
/// let broker = Arc::new(SyncFakeBrokerAdapter::new());
/// persisted(&store, "order-1", &machine, &MachineLogic::dynamic(), &options, |interp, session| {
///     interp.send("PAY")?;
///     // a broker sink is not transactional: publish only once the state is durable
///     let batch = plugin.drain();
///     let broker = Arc::clone(&broker);
///     session.after_commit(move || {
///         for env in &batch {
///             let _ = broker.publish("events", env);
///         }
///         Ok(())
///     });
///     Ok(())
/// })
/// # }
/// ```
///
/// With an [`OutboxStore`] call [`flush_into`](Self::flush_into) instead (in the body for
/// at-least-once with a relay, or in `after_commit` to never record a state that was not
/// kept), and [`discard`](Self::discard) when the step is abandoned.
///
/// One plugin per interpreter (or per dispatched envelope) is the intended use: the cause is
/// a single slot and the buffer is shared by everything the plugin observes.
pub struct OutboxPlugin {
    topic: String,
    redact_keys: RedactKeys,
    instance_key: Option<String>,
    cause: Mutex<Option<Envelope>>,
    buffer: Mutex<Vec<Envelope>>,
}

impl std::fmt::Debug for OutboxPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxPlugin")
            .field("topic", &self.topic)
            .field("instance_key", &self.instance_key)
            .field("buffered", &lock(&self.buffer).len())
            .finish_non_exhaustive()
    }
}

impl Default for OutboxPlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl OutboxPlugin {
    /// A plugin for topic `events` with the default redaction keys.
    #[must_use]
    pub fn new() -> Self {
        Self {
            topic: "events".into(),
            redact_keys: RedactKeys::default(),
            instance_key: None,
            cause: Mutex::new(None),
            buffer: Mutex::new(Vec::new()),
        }
    }

    /// The topic envelopes are written for (used by [`flush_into`](Self::flush_into)).
    #[must_use]
    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = topic.into();
        self
    }

    /// Keys masked in state-tag `data` (the whole context).
    #[must_use]
    pub fn redact_keys(mut self, keys: RedactKeys) -> Self {
        self.redact_keys = keys;
        self
    }

    /// The instance's store key, the subject fallback.
    #[must_use]
    pub fn instance_key(mut self, key: impl Into<String>) -> Self {
        self.instance_key = Some(key.into());
        self
    }

    /// The configured topic.
    #[must_use]
    pub fn topic_name(&self) -> &str {
        &self.topic
    }

    /// Set (or clear) the inbound envelope that caused the current step.
    pub fn set_cause(&self, cause: Option<Envelope>) {
        *lock(&self.cause) = cause;
    }

    /// Take every buffered envelope, oldest first.
    pub fn drain(&self) -> Vec<Envelope> {
        std::mem::take(&mut *lock(&self.buffer))
    }

    /// Drop buffered envelopes (the save failed); returns how many.
    pub fn discard(&self) -> usize {
        self.drain().len()
    }

    /// Number of buffered envelopes.
    #[must_use]
    pub fn buffered(&self) -> usize {
        lock(&self.buffer).len()
    }

    /// Drain into `store` under this plugin's topic; returns how many rows were added. On a
    /// store failure the rows not yet added are put back at the head of the buffer.
    ///
    /// # Errors
    /// The store's error.
    pub fn flush_into(&self, store: &dyn OutboxStore) -> Result<usize> {
        let batch = self.drain();
        for (i, env) in batch.iter().enumerate() {
            if let Err(e) = store.add(&self.topic, env) {
                let mut buf = lock(&self.buffer);
                let rest: Vec<Envelope> = batch[i..].iter().cloned().chain(buf.drain(..)).collect();
                *buf = rest;
                return Err(e);
            }
        }
        Ok(batch.len())
    }

    fn envelope(&self, machine: &Machine, ty: String, data: Value) -> Option<Envelope> {
        let cause = lock(&self.cause).clone();
        let origin = TransitionOrigin {
            machine,
            instance_key: self.instance_key.as_deref(),
            subject: None,
            source: None,
        };
        Envelope::from_transition(&origin, ty, data, cause.as_ref())
            .inspect_err(|e| tracing::error!(target: "xstate::eda", machine = machine.id(), error = %e, "outbox envelope refused"))
            .ok()
    }

    fn transition_envelope(
        &self,
        machine: &Machine,
        tid: TransitionId,
        ctx: &Value,
    ) -> Option<Envelope> {
        let (ty, fields) = match transition_spec(machine, machine.transition(tid)) {
            Ok(Some(spec)) => spec,
            Ok(None) => return None,
            Err(e) => {
                tracing::error!(target: "xstate::eda", machine = machine.id(), error = %e, "invalid meta.publish");
                return None;
            }
        };
        let data = fields.map_or(Value::Null, |fields| {
            Value::Object(
                fields
                    .into_iter()
                    .map(|f| {
                        let v = ctx.get(&f).cloned().unwrap_or(Value::Null);
                        (f, v)
                    })
                    .collect::<Map<_, _>>(),
            )
        });
        self.envelope(machine, ty, data)
    }

    fn state_envelope(&self, machine: &Machine, sid: StateId, ctx: &Value) -> Option<Envelope> {
        let data = if ctx.is_object() { self.redact_keys.redact(ctx) } else { Value::Null };
        self.envelope(machine, state_type(machine.id(), &machine.node(sid).id), data)
    }

    fn push(&self, out: Vec<Envelope>) {
        if !out.is_empty() {
            lock(&self.buffer).extend(out);
        }
    }
}

fn context_value<C: Context>(state: &ActiveState<C>) -> Value {
    serde_json::to_value(state.context()).unwrap_or(Value::Null)
}

impl<C: Context> Observer<C> for OutboxPlugin {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, report: &StepReport) {
        let ctx = context_value(state);
        let mut out: Vec<Envelope> = active_sorted(machine, state)
            .into_iter()
            .filter(|&s| has_tag(machine, s, PUBLISH_TAG))
            .filter_map(|s| self.state_envelope(machine, s, &ctx))
            .collect();
        out.extend(
            report.transitions.iter().filter_map(|&t| self.transition_envelope(machine, t, &ctx)),
        );
        self.push(out);
    }

    fn on_event(
        &self,
        machine: &Machine,
        _event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let ctx = context_value(state);
        let mut out = Vec::new();
        for (tid, entered) in entered_per_transition(machine, &report.transitions, state) {
            out.extend(self.transition_envelope(machine, tid, &ctx));
            out.extend(
                entered
                    .into_iter()
                    .filter(|&s| has_tag(machine, s, PUBLISH_TAG))
                    .filter_map(|s| self.state_envelope(machine, s, &ctx)),
            );
        }
        self.push(out);
    }
}
