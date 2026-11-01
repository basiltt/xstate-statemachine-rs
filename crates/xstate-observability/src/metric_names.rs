//! Metric and label names, plus [`describe_metrics`].
//!
//! The names follow the Python `PrometheusPlugin` (`xstatemachine_` prefix). The `metrics`
//! facade does not append `_total` to counters the way `prometheus_client` does, so counter
//! names carry the suffix explicitly; a Prometheus scrape therefore shows the same series
//! names a Python deployment exposes.
//!
//! | Constant | Kind | Labels | Python metric |
//! |---|---|---|---|
//! | [`TRANSITIONS`] | counter | `machine`, `from_state`, `to_state`, `event` | `xstatemachine_transitions` (same labels) |
//! | [`EVENTS_RECEIVED`] | counter | `machine`, `event`, `disposition` | `xstatemachine_events_received` (same labels; `disposition` is `handled`, `unhandled` or `dropped`) |
//! | [`TRANSITION_DURATION_SECONDS`] | histogram | `machine` | `xstatemachine_transition_duration_seconds` (only via [`MetricsObserver::timed`](crate::MetricsObserver::timed)) |
//! | [`ACTIVE_INTERPRETERS`] | gauge | `machine` | `xstatemachine_active_interpreters` (event driven here, polled in Python) |
//! | [`CHAIN_TRIPS`] | counter | `machine` | `xstatemachine_chain_trips` |
//! | [`ACTIONS`] | counter | `machine`, `action` | none (Rust only) |
//! | [`UNHANDLED_EVENTS`] | counter | `machine`, `event` | none (Python: `events_received{disposition="unhandled"}`) |
//! | [`EVENTS_DROPPED`] | counter | `machine`, `reason` | none (Python: `events_received{disposition="dropped"}`) |
//! | [`ERRORS`] | counter | `machine`, `kind` | none (Rust only) |
//! | [`MICROSTEPS`] | histogram | `machine` | none (Rust only) |
//!
//! Python metrics with no counterpart because the hooks do not expose them:
//! `guard_evaluations`, `guard_errors`, `action_errors`, `service_duration_seconds`,
//! `service_errors` and the polled `queue_depth`.
//!
//! Without the machine label ([`MetricsObserver::with_machine_label`](crate::MetricsObserver::with_machine_label)
//! set to `false`) the `machine` label is omitted, as with Python's `labels=()`.

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};

/// Counter: transitions taken, one increment per entry of `StepReport::transitions`.
pub const TRANSITIONS: &str = "xstatemachine_transitions_total";
/// Counter: events that entered the machine, by disposition.
pub const EVENTS_RECEIVED: &str = "xstatemachine_events_received_total";
/// Histogram: seconds to process one event (recorded by `MetricsObserver::timed`).
pub const TRANSITION_DURATION_SECONDS: &str = "xstatemachine_transition_duration_seconds";
/// Gauge: interpreters currently running.
pub const ACTIVE_INTERPRETERS: &str = "xstatemachine_active_interpreters";
/// Counter: runaway-chain budget trips.
pub const CHAIN_TRIPS: &str = "xstatemachine_chain_trips_total";
/// Counter: user actions executed, by action name.
pub const ACTIONS: &str = "xstatemachine_actions_total";
/// Counter: events that no transition handled.
pub const UNHANDLED_EVENTS: &str = "xstatemachine_unhandled_events_total";
/// Counter: events dropped without being processed, by reason.
pub const EVENTS_DROPPED: &str = "xstatemachine_events_dropped_total";
/// Counter: step errors, by error kind.
pub const ERRORS: &str = "xstatemachine_errors_total";
/// Histogram: microsteps executed per processed event.
pub const MICROSTEPS: &str = "xstatemachine_microsteps";

/// Label: the chart id.
pub const LABEL_MACHINE: &str = "machine";
/// Label: the (allow-listed) event type.
pub const LABEL_EVENT: &str = "event";
/// Label: the source state id of a transition.
pub const LABEL_FROM_STATE: &str = "from_state";
/// Label: the target state id of a transition (the source for a targetless one).
pub const LABEL_TO_STATE: &str = "to_state";
/// Label: what happened to an event: `handled`, `unhandled` or `dropped`.
pub const LABEL_DISPOSITION: &str = "disposition";
/// Label: the action name.
pub const LABEL_ACTION: &str = "action";
/// Label: the short machine-readable drop reason.
pub const LABEL_REASON: &str = "reason";
/// Label: the `ErrorKind` of a step error.
pub const LABEL_KIND: &str = "kind";

/// Register help texts and units for every metric with the installed recorder.
///
/// Call once at start-up, after installing the exporter. Help texts for the metrics shared
/// with Python are the Python ones.
pub fn describe_metrics() {
    describe_counter!(TRANSITIONS, Unit::Count, "Transitions taken.");
    describe_counter!(
        EVENTS_RECEIVED,
        Unit::Count,
        "Events that entered the machine, by disposition."
    );
    describe_histogram!(
        TRANSITION_DURATION_SECONDS,
        Unit::Seconds,
        "Wall time to process one event (received -> settled)."
    );
    describe_gauge!(ACTIVE_INTERPRETERS, Unit::Count, "Interpreters currently running.");
    describe_counter!(CHAIN_TRIPS, Unit::Count, "Runaway-chain budget trips.");
    describe_counter!(ACTIONS, Unit::Count, "User actions executed.");
    describe_counter!(UNHANDLED_EVENTS, Unit::Count, "Events no transition handled.");
    describe_counter!(
        EVENTS_DROPPED,
        Unit::Count,
        "Events dropped without being processed, by reason."
    );
    describe_counter!(ERRORS, Unit::Count, "Step errors, by error kind.");
    describe_histogram!(MICROSTEPS, Unit::Count, "Microsteps executed per processed event.");
}
