//! [`DeadLetterPlugin`]: capture a poison message with its error context when the chart
//! enters a dead-letter state (Python `patterns.dead_letter.DeadLetterPlugin`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};
use xstate_core::redact::RedactKeys;
use xstate_core::snapshot::{Snapshot, python_event_name};
use xstate_core::{
    ActiveState, Context, Error, Event, EventOrigin, Machine, Observer, StateId, StepReport,
};
use xstate_persistence::receipts::ReceiptErrorKind;

use super::{
    DEAD_LETTER_TAG, DEFAULT_MAX_ERRORS, DeadLetter, DeadLetterSink, ERRORS_CONTEXT_KEY,
    ErrorEntry, MAX_ERROR_MESSAGE,
};
use crate::chart::{active_sorted, entered_per_transition, has_tag};
use crate::error::{EdaError, Result};

/// Emit a [`DeadLetter`] when the machine enters a dead-letter state: a state tagged
/// `dead-letter`, or one of the explicit [`state_ids`](Self::state_ids).
///
/// The record holds the event being handled, the attempt counter (`context[attempt_key]`),
/// the error chain since the last clean step and a redacted snapshot. Errors are collected
/// from the hooks Rust observers get:
///
/// * an `xstate.error.actor.*` event (a failed service) → `{"source": "service", "name": <src>}`;
/// * a step whose action failed and was rolled back (`report.failed`) and
///   [`Observer::on_error`] → `{"source": "action"}`;
/// * an `xstate.done.actor.*` event (a service succeeded) clears the chain.
///
/// Deviations from Python, forced by the observer API: an observer cannot write the context,
/// so the chain lives in the plugin (one plugin per interpreter; it does not survive a
/// `persisted()` boundary). A chain the chart keeps itself under [`ERRORS_CONTEXT_KEY`] is
/// merged in front of it, and that key is dropped from the recorded snapshot. The snapshot is
/// built from the observed state (`Snapshot::from_state`), so it carries no timers or queues.
/// A failing sink is logged with the record id; it never stops the machine.
pub struct DeadLetterPlugin {
    sink: Arc<dyn DeadLetterSink>,
    state_ids: HashSet<String>,
    attempt_key: String,
    redact_keys: RedactKeys,
    include_snapshot: bool,
    max_errors: usize,
    instance_id: Option<String>,
    errors: Mutex<Vec<ErrorEntry>>,
}

impl std::fmt::Debug for DeadLetterPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadLetterPlugin")
            .field("state_ids", &self.state_ids)
            .field("attempt_key", &self.attempt_key)
            .field("include_snapshot", &self.include_snapshot)
            .field("max_errors", &self.max_errors)
            .finish_non_exhaustive()
    }
}

impl DeadLetterPlugin {
    /// A plugin writing to `sink`, watching `dead-letter`-tagged states.
    pub fn new(sink: Arc<dyn DeadLetterSink>) -> Self {
        Self {
            sink,
            state_ids: HashSet::new(),
            attempt_key: "attempt".into(),
            redact_keys: RedactKeys::default(),
            include_snapshot: true,
            max_errors: DEFAULT_MAX_ERRORS,
            instance_id: None,
            errors: Mutex::new(Vec::new()),
        }
    }

    /// Explicit dead-letter state ids (full dotted ids); the tag is then ignored.
    #[must_use]
    pub fn state_ids<I: IntoIterator<Item = S>, S: Into<String>>(mut self, ids: I) -> Self {
        self.state_ids = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Context key holding the attempt counter (default `attempt`).
    #[must_use]
    pub fn attempt_key(mut self, key: impl Into<String>) -> Self {
        self.attempt_key = key.into();
        self
    }

    /// Keys masked in payloads, snapshots and error messages.
    #[must_use]
    pub fn redact_keys(mut self, keys: RedactKeys) -> Self {
        self.redact_keys = keys;
        self
    }

    /// `false` omits the snapshot (smaller records; no replay-from-state).
    #[must_use]
    pub fn include_snapshot(mut self, include: bool) -> Self {
        self.include_snapshot = include;
        self
    }

    /// Keep only the newest `n` errors of the chain.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for 0.
    pub fn max_errors(mut self, n: usize) -> Result<Self> {
        if n < 1 {
            return Err(EdaError::InvalidArgument("max_errors must be >= 1".into()));
        }
        self.max_errors = n;
        Ok(self)
    }

    /// The record's `machine_id` (Python `interpreter.id`; default the machine id).
    #[must_use]
    pub fn instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = Some(id.into());
        self
    }

    fn chain(&self) -> MutexGuard<'_, Vec<ErrorEntry>> {
        self.errors.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, source: &str, name: &str, ty: &str, message: &str, ctx: &Value) {
        let mut message = scrub(message, ctx, &self.redact_keys);
        if message.chars().count() > MAX_ERROR_MESSAGE {
            message = message.chars().take(MAX_ERROR_MESSAGE).collect::<String>() + "…";
        }
        let mut chain = self.chain();
        chain.push(ErrorEntry::new(source, name, ty, message));
        let excess = chain.len().saturating_sub(self.max_errors);
        chain.drain(..excess);
    }

    fn is_dead_letter(&self, machine: &Machine, sid: StateId) -> bool {
        if self.state_ids.is_empty() {
            has_tag(machine, sid, DEAD_LETTER_TAG)
        } else {
            self.state_ids.contains(&*machine.node(sid).id)
        }
    }

    fn collect_errors(&self, machine: &Machine, event: &Event, report: &StepReport, ctx: &Value) {
        match event.origin {
            EventOrigin::DoneActor => self.chain().clear(),
            EventOrigin::ErrorActor => {
                let invoke = event.ty.strip_prefix("xstate.error.actor.").unwrap_or(&event.ty);
                let name = machine
                    .states()
                    .iter()
                    .flat_map(|s| s.invokes.iter())
                    .find(|i| i.id == invoke)
                    .map_or(invoke, |i| i.src.as_str());
                let (ty, message) = service_error(event.payload.get("error"));
                self.push("service", name, &ty, &message, ctx);
            }
            _ => {}
        }
        if let Some(failed) = &report.failed {
            self.push("action", "?", "TransitionFailedError", failed, ctx);
        }
    }

    fn capture<C: Context>(
        &self,
        machine: &Machine,
        event: &Event,
        state: &ActiveState<C>,
        ctx: &Value,
        sid: StateId,
    ) -> DeadLetter {
        let mut errors: Vec<ErrorEntry> = ctx
            .get(ERRORS_CONTEXT_KEY)
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().filter_map(|e| serde_json::from_value(e.clone()).ok()).collect()
            })
            .unwrap_or_default();
        errors.extend(std::mem::take(&mut *self.chain()));
        let mut ev = Map::new();
        ev.insert("type".into(), Value::from(python_event_name(&event.ty)));
        let payload = match &event.payload {
            Value::Object(m) => self.redact_keys.redact(&Value::Object(m.clone())),
            _ => Value::Object(Map::new()),
        };
        ev.insert("payload".into(), payload);
        let mut rec = DeadLetter::new(
            self.instance_id.clone().unwrap_or_else(|| machine.id().to_owned()),
            machine.node(sid).id.to_string(),
            Value::Object(ev),
            crate::py::wall_now(),
        );
        rec.attempts = ctx.get(&self.attempt_key).and_then(attempt_number);
        rec.errors = errors;
        if self.include_snapshot {
            rec.snapshot = self.snapshot(machine, state);
        }
        rec.machine_hash = Some(machine.structure_hash().to_owned());
        rec.machine_version = machine.version().map(str::to_owned);
        rec
    }

    fn snapshot<C: Context>(&self, machine: &Machine, state: &ActiveState<C>) -> Value {
        let Ok(mut snap) = Snapshot::from_state(machine, state) else {
            return Value::Object(Map::new());
        };
        snap.taken_at = crate::py::wall_now();
        let mut value = snap
            .to_json()
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or_else(|| Value::Object(Map::new()));
        self.redact_keys.redact_in_place(&mut value);
        if let Some(Value::Object(ctx)) = value.get_mut("context") {
            ctx.shift_remove(ERRORS_CONTEXT_KEY);
        }
        value
    }

    fn emit<C: Context>(
        &self,
        machine: &Machine,
        event: &Event,
        state: &ActiveState<C>,
        ctx: &Value,
        entered: &[StateId],
    ) {
        let mut ids: Vec<StateId> =
            entered.iter().copied().filter(|&s| self.is_dead_letter(machine, s)).collect();
        ids.sort_by(|a, b| machine.node(*a).id.cmp(&machine.node(*b).id));
        ids.dedup();
        for sid in ids {
            let record = self.capture(machine, event, state, ctx, sid);
            let (id, state_id) = (record.id.clone(), record.state_id.clone());
            if let Err(e) = self.sink.put(record) {
                tracing::error!(target: "xstate::eda", record = %id, machine = machine.id(), state = %state_id, error = %e, "DeadLetter sink failed; the record was NOT stored");
            }
        }
    }
}

/// Python `type(err).__name__` / `str(err)` for a service error payload.
fn service_error(error: Option<&Value>) -> (String, String) {
    match error {
        Some(Value::String(s)) => ("Error".into(), s.clone()),
        Some(Value::Object(m)) => {
            let ty =
                m.get("type").or_else(|| m.get("name")).and_then(Value::as_str).unwrap_or("Error");
            let message = m
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| Value::Object(m.clone()).to_string(), str::to_owned);
            (ty.to_owned(), message)
        }
        Some(other) => ("Error".into(), other.to_string()),
        None => ("Error".into(), String::new()),
    }
}

/// Python `int(context[attempt_key])`, `None` when it does not convert.
// Truncating a float towards zero is exactly Python's `int(float)`; out-of-range values
// saturate instead of raising, which is harmless for an attempt counter.
#[allow(clippy::cast_possible_truncation)]
fn attempt_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64))
        }
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// Mask secret context values that leak into an error message: every string value (4+
/// characters) whose key the redactor would mask is replaced by `***`.
fn scrub(message: &str, ctx: &Value, keys: &RedactKeys) -> String {
    let Value::Object(map) = ctx else { return message.to_owned() };
    let mut out = message.to_owned();
    for (k, v) in map {
        if let Value::String(s) = v
            && s.chars().count() >= 4
            && keys.matches(k)
            && out.contains(s.as_str())
        {
            out = out.replace(s.as_str(), "***");
        }
    }
    out
}

fn context_value<C: Context>(state: &ActiveState<C>) -> Value {
    serde_json::to_value(state.context()).unwrap_or(Value::Null)
}

impl<C: Context> Observer<C> for DeadLetterPlugin {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, _report: &StepReport) {
        let ctx = context_value(state);
        let entered = active_sorted(machine, state);
        self.emit(machine, &Event::init(), state, &ctx, &entered);
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let ctx = context_value(state);
        self.collect_errors(machine, event, report, &ctx);
        let entered: Vec<StateId> = entered_per_transition(machine, &report.transitions, state)
            .into_iter()
            .flat_map(|(_, states)| states)
            .collect();
        self.emit(machine, event, state, &ctx, &entered);
    }

    fn on_error(&self, _machine: &Machine, error: &Error) {
        let ty = ReceiptErrorKind::from_engine(error.kind());
        self.push("action", "?", ty.type_name(), &error.to_string(), &Value::Null);
    }

    fn on_stop(&self, _machine: &Machine, _state: &ActiveState<C>) {
        self.chain().clear();
    }
}
