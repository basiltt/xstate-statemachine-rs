//! [`MetricsObserver`]: an [`Observer`] that records the metric set of the Python
//! `PrometheusPlugin` through the [`metrics`] facade.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use metrics::{Label, counter, gauge, histogram};
use xstate_core::machine::Trigger;
use xstate_core::{
    ActiveState, Context, Error, ErrorKind, Event, Machine, Observer, Status, StepReport,
};

use crate::hygiene::{ALWAYS, LabelGuard, event_label};
use crate::metric_names::{
    ACTIONS, ACTIVE_INTERPRETERS, CHAIN_TRIPS, ERRORS, EVENTS_DROPPED, EVENTS_RECEIVED,
    LABEL_ACTION, LABEL_DISPOSITION, LABEL_EVENT, LABEL_FROM_STATE, LABEL_KIND, LABEL_MACHINE,
    LABEL_REASON, LABEL_TO_STATE, MICROSTEPS, TRANSITION_DURATION_SECONDS, TRANSITIONS,
    UNHANDLED_EVENTS,
};

/// Drop reason reported by interpreters for an event sent to a machine that is not running.
const REASON_NOT_RUNNING: &str = "not_running";

/// Records lifecycle metrics through the [`metrics`] facade.
///
/// Attach it with `SyncInterpreter::observe` (or the tokio interpreter's equivalent); install
/// whichever exporter the application prefers (for example `metrics-exporter-prometheus`,
/// which this crate deliberately does not depend on). Call [`describe_metrics`](crate::describe_metrics)
/// once to register help texts. See [`metric_names`](crate::metric_names) for the metric set and
/// its mapping to the Python plugin.
///
/// Every label value passes through the telemetry hygiene rules ([`LabelGuard`]): event types
/// must be declared by the chart (else `unknown`), and each label dimension is capped
/// (overflow is `other`). Payloads, context and instance keys are never recorded.
///
/// # Limitations
///
/// * Observer hooks carry no interpreter identity, so `active_interpreters` is maintained by
///   counting starts and terminal transitions (stop, done, error) per machine inside this
///   observer, and never decremented below zero. Use one observer per interpreter when you
///   need exact numbers; calling `stop` twice on one interpreter is counted twice.
/// * `unhandled` under `onUnhandled: error` flips the machine to `error` after `on_event`
///   returns, which no hook sees, so that exit does not decrement the gauge.
/// * Transition *duration* cannot be measured from the hooks (they fire after the event has
///   been processed). Wrap the call that drives the machine in [`MetricsObserver::timed`].
/// * Guard, service and queue-depth metrics of the Python plugin have no hook here.
#[derive(Debug)]
pub struct MetricsObserver {
    guard: LabelGuard,
    machine_label: bool,
    live: Mutex<HashMap<String, u64>>,
}

impl Default for MetricsObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsObserver {
    /// An observer labelling every series with the machine id and capping each label dimension
    /// at 100 distinct values.
    #[must_use]
    pub fn new() -> Self {
        Self { guard: LabelGuard::default(), machine_label: true, live: Mutex::new(HashMap::new()) }
    }

    /// Cap every label dimension at `max_label_values` distinct values (overflow is `other`).
    #[must_use]
    pub fn with_max_label_values(mut self, max_label_values: NonZeroUsize) -> Self {
        self.guard = LabelGuard::new(max_label_values);
        self
    }

    /// Whether to label series with the machine id (default `true`). `false` drops the
    /// `machine` label, like `labels=()` in Python.
    #[must_use]
    pub fn with_machine_label(mut self, enabled: bool) -> Self {
        self.machine_label = enabled;
        self
    }

    /// The configured per-dimension cap.
    #[must_use]
    pub fn max_label_values(&self) -> usize {
        self.guard.max_label_values()
    }

    /// Run `f` and record its wall time as `xstatemachine_transition_duration_seconds` for
    /// `machine`.
    ///
    /// Wrap the call that processes an event (`interpreter.send(..)`). The observer hooks run
    /// after the work is done and cannot see when it began, so durations need this helper. The
    /// observation is skipped if `f` panics.
    pub fn timed<T>(&self, machine: &Machine, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        histogram!(TRANSITION_DURATION_SECONDS, self.labels(machine, &[]))
            .record(started.elapsed().as_secs_f64());
        out
    }

    /// The guarded machine label value, or `None` when the machine label is disabled.
    fn machine_value(&self, machine: &Machine) -> Option<String> {
        self.machine_label.then(|| self.guard.label(LABEL_MACHINE, machine.id()))
    }

    /// Base labels plus `extra`, every extra value already guarded by the caller.
    fn labels(&self, machine: &Machine, extra: &[(&'static str, &str)]) -> Vec<Label> {
        let mut out = Vec::with_capacity(extra.len() + 1);
        if let Some(value) = self.machine_value(machine) {
            out.push(Label::new(LABEL_MACHINE, value));
        }
        out.extend(extra.iter().map(|(key, value)| Label::new(*key, (*value).to_owned())));
        out
    }

    fn event_value(&self, machine: &Machine, event_type: &str) -> String {
        self.guard.label(LABEL_EVENT, &event_label(machine, event_type))
    }

    /// Label for the trigger of a taken transition. Descriptors come from the chart, so
    /// wildcards (`*`, `foo.*`) pass through; everything else follows [`event_label`].
    fn trigger_value(&self, machine: &Machine, trigger: &Trigger) -> String {
        let label = match trigger {
            Trigger::Always => ALWAYS.to_owned(),
            Trigger::Event(descriptor) if descriptor.ends_with('*') => descriptor.to_string(),
            Trigger::Event(descriptor) => event_label(machine, descriptor),
        };
        self.guard.label(LABEL_EVENT, &label)
    }

    /// Record the transitions, actions and microsteps of one report.
    fn record_report(&self, machine: &Machine, report: &StepReport) {
        for id in &report.transitions {
            let transition = machine.transition(*id);
            let from = machine.node(transition.source).id.to_string();
            let to = transition
                .targets
                .first()
                .map_or_else(|| from.clone(), |target| machine.node(*target).id.to_string());
            let event = self.trigger_value(machine, &transition.trigger);
            let from = self.guard.label(LABEL_FROM_STATE, &from);
            let to = self.guard.label(LABEL_TO_STATE, &to);
            let labels = self.labels(
                machine,
                &[(LABEL_FROM_STATE, &from), (LABEL_TO_STATE, &to), (LABEL_EVENT, &event)],
            );
            counter!(TRANSITIONS, labels).increment(1);
        }
        for action in &report.actions {
            let action = self.guard.label(LABEL_ACTION, action);
            counter!(ACTIONS, self.labels(machine, &[(LABEL_ACTION, &action)])).increment(1);
        }
        histogram!(MICROSTEPS, self.labels(machine, &[])).record(f64::from(report.microsteps));
    }

    fn mark_started(&self, machine: &Machine) {
        let key = self.machine_value(machine).unwrap_or_default();
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        *live.entry(key).or_default() += 1;
        gauge!(ACTIVE_INTERPRETERS, self.labels(machine, &[])).increment(1.0);
    }

    /// Decrement the gauge, never below the number of starts this observer has seen.
    fn mark_ended(&self, machine: &Machine) {
        let key = self.machine_value(machine).unwrap_or_default();
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(count) = live.get_mut(&key).filter(|count| **count > 0) else {
            return;
        };
        *count -= 1;
        gauge!(ACTIVE_INTERPRETERS, self.labels(machine, &[])).decrement(1.0);
    }
}

impl<C: Context> Observer<C> for MetricsObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, report: &StepReport) {
        self.record_report(machine, report);
        if state.status() == Status::Running {
            self.mark_started(machine);
        }
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let event_value = self.event_value(machine, &event.ty);
        let disposition = if report.unhandled { "unhandled" } else { "handled" };
        counter!(
            EVENTS_RECEIVED,
            self.labels(machine, &[(LABEL_EVENT, &event_value), (LABEL_DISPOSITION, disposition)])
        )
        .increment(1);
        if report.unhandled {
            counter!(UNHANDLED_EVENTS, self.labels(machine, &[(LABEL_EVENT, &event_value)]))
                .increment(1);
        }
        self.record_report(machine, report);
        if matches!(state.status(), Status::Done | Status::Error) {
            self.mark_ended(machine);
        }
    }

    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {
        let reason_value = self.guard.label(LABEL_REASON, reason);
        counter!(EVENTS_DROPPED, self.labels(machine, &[(LABEL_REASON, &reason_value)]))
            .increment(1);
        // Deferred events were already counted when they were processed as unhandled; only
        // events that never entered the machine get the `dropped` disposition.
        if reason == REASON_NOT_RUNNING {
            let event_value = self.event_value(machine, &event.ty);
            counter!(
                EVENTS_RECEIVED,
                self.labels(
                    machine,
                    &[(LABEL_EVENT, &event_value), (LABEL_DISPOSITION, "dropped")]
                )
            )
            .increment(1);
        }
    }

    fn on_error(&self, machine: &Machine, error: &Error) {
        let kind = error.kind();
        counter!(ERRORS, self.labels(machine, &[(LABEL_KIND, &kind.to_string())])).increment(1);
        if kind == ErrorKind::RunawayChain {
            counter!(CHAIN_TRIPS, self.labels(machine, &[])).increment(1);
        }
        self.mark_ended(machine);
    }

    fn on_stop(&self, machine: &Machine, state: &ActiveState<C>) {
        // A machine that already finished (done / error) was accounted for when it did.
        if state.status() == Status::Stopped {
            self.mark_ended(machine);
        }
    }
}
