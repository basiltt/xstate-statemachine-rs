//! [`OutboxPlugin`]: turn chart-declared transitions and `publish`-tagged states into outbound
//! envelopes (Python `OutboxPlugin`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};
use xstate_core::redact::RedactKeys;
use xstate_core::{
    ActiveState, Context, Event, Machine, Observer, StateId, StepReport, TransitionId,
};

use super::specs::{publish_specs, state_type, transition_spec};
use super::{OutboxStore, PUBLISH_TAG};
use crate::chart::{LazyContext, active_sorted, entered_per_transition, has_tag};
use crate::envelope::{Envelope, TransitionOrigin};
use crate::error::{EdaError, Result};

/// Envelopes the plugin buffers before it starts dropping new ones (see
/// [`OutboxPlugin::max_buffered`]).
pub const DEFAULT_MAX_BUFFERED: usize = 10_000;

/// Failures the plugin remembers; the oldest are forgotten beyond this.
const MAX_FAILURES: usize = 1_000;

/// One envelope the plugin could not produce (see [`OutboxPlugin::failures`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxFailure {
    /// The machine that was observed.
    pub machine: String,
    /// The envelope type that was meant to be published (`""` when it is not known).
    pub ty: String,
    /// Why nothing was published.
    pub reason: String,
}

/// A parsed `meta.publish`: the type and the context fields copied into `data`.
type Spec = (String, Option<Vec<String>>);

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
/// # Failures are never silent
///
/// An observer cannot return an error, so whatever goes wrong is logged (`tracing::error!`)
/// and kept for the caller: [`failures`](Self::failures) lists the envelopes that were
/// refused, whose context could not be serialised (nothing is published with `data: null` in
/// its place) or whose chart declares a malformed `meta.publish`, and
/// [`try_drain`](Self::try_drain) returns the malformed-chart error instead of an empty batch.
/// The buffer is bounded ([`max_buffered`](Self::max_buffered)): envelopes over the bound are
/// dropped and counted ([`dropped`](Self::dropped)).
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
    max_buffered: usize,
    cause: Mutex<Option<Envelope>>,
    buffer: Mutex<Vec<Envelope>>,
    failures: Mutex<Vec<OutboxFailure>>,
    spec_error: Mutex<Option<String>>,
    specs_checked: AtomicBool,
    dropped: AtomicU64,
}

impl std::fmt::Debug for OutboxPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxPlugin")
            .field("topic", &self.topic)
            .field("instance_key", &self.instance_key)
            .field("buffered", &lock(&self.buffer).len())
            .field("failures", &lock(&self.failures).len())
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
    /// A plugin for topic `events` with the default redaction keys and a buffer of
    /// [`DEFAULT_MAX_BUFFERED`] envelopes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            topic: "events".into(),
            redact_keys: RedactKeys::default(),
            instance_key: None,
            max_buffered: DEFAULT_MAX_BUFFERED,
            cause: Mutex::new(None),
            buffer: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            spec_error: Mutex::new(None),
            specs_checked: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
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

    /// Buffer at most `n` envelopes (default [`DEFAULT_MAX_BUFFERED`]); what arrives while the
    /// buffer is full is dropped, logged and counted in [`dropped`](Self::dropped), so a caller
    /// that never drains cannot grow the process without bound.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for 0.
    pub fn max_buffered(mut self, n: usize) -> Result<Self> {
        if n < 1 {
            return Err(EdaError::InvalidArgument("max_buffered must be >= 1".into()));
        }
        self.max_buffered = n;
        Ok(self)
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
    ///
    /// This cannot fail: a chart whose `meta.publish` is malformed simply yields nothing for
    /// the malformed part. Use [`try_drain`](Self::try_drain) to be told.
    pub fn drain(&self) -> Vec<Envelope> {
        std::mem::take(&mut *lock(&self.buffer))
    }

    /// [`drain`](Self::drain), but a malformed `meta.publish` (found once, when the plugin
    /// first observed the machine) is returned instead of an incomplete batch; the buffer is
    /// left untouched then.
    ///
    /// # Errors
    /// [`EdaError::InvalidSpec`] when the chart's `meta.publish` is malformed.
    pub fn try_drain(&self) -> Result<Vec<Envelope>> {
        if let Some(why) = lock(&self.spec_error).clone() {
            return Err(EdaError::InvalidSpec(why));
        }
        Ok(self.drain())
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

    /// Envelopes dropped because the buffer was full, over the plugin's life.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The envelopes that could not be produced, oldest first (at most the newest 1000 are
    /// kept). Each is also logged with `tracing::error!`.
    #[must_use]
    pub fn failures(&self) -> Vec<OutboxFailure> {
        lock(&self.failures).clone()
    }

    /// Take the recorded [`failures`](Self::failures), leaving the list empty.
    pub fn take_failures(&self) -> Vec<OutboxFailure> {
        std::mem::take(&mut *lock(&self.failures))
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

    /// Log and remember an envelope that was not produced.
    fn fail(&self, machine: &Machine, ty: &str, reason: String) {
        tracing::error!(
            target: "xstate::eda",
            machine = machine.id(),
            ty,
            reason = %reason,
            "outbox: nothing was published"
        );
        let mut failures = lock(&self.failures);
        if failures.len() >= MAX_FAILURES {
            failures.remove(0);
        }
        failures.push(OutboxFailure {
            machine: machine.id().to_owned(),
            ty: ty.to_owned(),
            reason,
        });
    }

    /// Validate the chart's `meta.publish` declarations once; the error stays for
    /// [`try_drain`](Self::try_drain).
    fn check_specs(&self, machine: &Machine) {
        if self.specs_checked.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(e) = publish_specs(machine) {
            let why = e.to_string();
            self.fail(machine, "", format!("invalid meta.publish: {why}"));
            *lock(&self.spec_error) = Some(why);
        }
    }

    /// The `meta.publish` of a transition; a malformed one was already reported by
    /// [`check_specs`](Self::check_specs).
    fn transition_spec_of(&self, machine: &Machine, tid: TransitionId) -> Option<Spec> {
        match transition_spec(machine, machine.transition(tid)) {
            Ok(spec) => spec,
            Err(e) => {
                if lock(&self.spec_error).is_none() {
                    self.fail(machine, "", format!("invalid meta.publish: {e}"));
                }
                None
            }
        }
    }

    fn envelope(&self, machine: &Machine, ty: &str, data: Value) -> Option<Envelope> {
        let cause = lock(&self.cause).clone();
        let origin = TransitionOrigin {
            machine,
            instance_key: self.instance_key.as_deref(),
            subject: None,
            source: None,
        };
        match Envelope::from_transition(&origin, ty.to_owned(), data, cause.as_ref()) {
            Ok(envelope) => Some(envelope),
            Err(e) => {
                self.fail(machine, ty, format!("the envelope was refused: {e}"));
                None
            }
        }
    }

    /// The envelopes of one transition's `spec` and of the `states` it entered. The context is
    /// serialised only when there is something to publish; if it cannot be, nothing is
    /// published (never `data: null`) and the failure is recorded.
    fn build<C: Context>(
        &self,
        machine: &Machine,
        ctx: &LazyContext<'_, C>,
        spec: Option<Spec>,
        states: &[StateId],
    ) -> Vec<Envelope> {
        if spec.is_none() && states.is_empty() {
            return Vec::new();
        }
        let ctx = match ctx.get() {
            Ok(ctx) => ctx,
            Err(why) => {
                let ty = spec.as_ref().map_or("", |(ty, _)| ty.as_str());
                self.fail(machine, ty, format!("the context could not be serialised: {why}"));
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        if let Some((ty, fields)) = spec {
            let (data, missing) = spec_data(fields, ctx);
            if !missing.is_empty() {
                // a misspelled field would publish `null` and nobody would know
                tracing::warn!(
                    target: "xstate::eda",
                    machine = machine.id(),
                    ty = %ty,
                    ?missing,
                    "meta.publish: context has no such field(s); published as null"
                );
            }
            out.extend(self.envelope(machine, &ty, data));
        }
        for &sid in states {
            let data = if ctx.is_object() { self.redact_keys.redact(ctx) } else { Value::Null };
            let ty = state_type(machine.id(), &machine.node(sid).id);
            out.extend(self.envelope(machine, &ty, data));
        }
        out
    }

    /// Buffer `out`, dropping (and counting) what does not fit.
    fn push(&self, out: Vec<Envelope>) {
        if out.is_empty() {
            return;
        }
        let mut buffer = lock(&self.buffer);
        let room = self.max_buffered.saturating_sub(buffer.len());
        let lost = out.len().saturating_sub(room);
        buffer.extend(out.into_iter().take(room));
        drop(buffer);
        if lost > 0 {
            let lost = u64::try_from(lost).unwrap_or(u64::MAX);
            self.dropped.fetch_add(lost, Ordering::Relaxed);
            tracing::error!(
                target: "xstate::eda",
                lost,
                max_buffered = self.max_buffered,
                "outbox buffer full: envelopes were dropped (drain it, or raise max_buffered)"
            );
        }
    }
}

/// `data` of a transition envelope: the listed context fields (or absent), and the listed
/// fields the context does not have.
fn spec_data(fields: Option<Vec<String>>, ctx: &Value) -> (Value, Vec<String>) {
    let Some(fields) = fields else { return (Value::Null, Vec::new()) };
    let mut missing = Vec::new();
    let mut data = Map::new();
    for f in fields {
        let value = ctx.get(&f).cloned().unwrap_or_else(|| {
            missing.push(f.clone());
            Value::Null
        });
        data.insert(f, value);
    }
    (Value::Object(data), missing)
}

impl<C: Context> Observer<C> for OutboxPlugin {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, report: &StepReport) {
        self.check_specs(machine);
        let tagged: Vec<StateId> = active_sorted(machine, state)
            .into_iter()
            .filter(|&s| has_tag(machine, s, PUBLISH_TAG))
            .collect();
        let specs: Vec<Spec> = report
            .transitions
            .iter()
            .filter_map(|&t| self.transition_spec_of(machine, t))
            .collect();
        let ctx = LazyContext::new(state);
        let mut out = self.build(machine, &ctx, None, &tagged);
        for spec in specs {
            out.extend(self.build(machine, &ctx, Some(spec), &[]));
        }
        self.push(out);
    }

    fn on_event(
        &self,
        machine: &Machine,
        _event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        self.check_specs(machine);
        let ctx = LazyContext::new(state);
        let mut out = Vec::new();
        for (tid, entered) in entered_per_transition(machine, &report.transitions, state) {
            let tagged: Vec<StateId> =
                entered.into_iter().filter(|&s| has_tag(machine, s, PUBLISH_TAG)).collect();
            let spec = self.transition_spec_of(machine, tid);
            out.extend(self.build(machine, &ctx, spec, &tagged));
        }
        self.push(out);
    }
}
