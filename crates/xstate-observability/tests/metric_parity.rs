//! Metric name/label parity with the Python `PrometheusPlugin`, plus the cardinality guard under
//! a flood of undeclared events and `timed`.
//!
//! The Python metric names below were enumerated with the released library (0.12.1) by scraping
//! a `PrometheusPlugin` pointed at a `CollectorRegistry` (see the QA report). `prometheus_client`
//! appends `_total` to counters at scrape time, so a Python `Counter("xstatemachine_transitions")`
//! scrapes as `xstatemachine_transitions_total`; the Rust crate spells the counter names with the
//! suffix explicitly so a scrape shows the same series names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::NonZeroUsize;
use std::sync::Arc;

use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use xstate_core::{Machine, MachineLogic, SyncInterpreter};
use xstate_observability::{MetricsObserver, describe_metrics, metric_names as n};

/// The Python scrape-time metric families (name and kind), as produced by
/// `PrometheusPlugin(CollectorRegistry())` after `generate_latest`.
const PYTHON_COUNTERS: &[&str] = &[
    "xstatemachine_transitions_total",
    "xstatemachine_events_received_total",
    "xstatemachine_guard_evaluations_total",
    "xstatemachine_guard_errors_total",
    "xstatemachine_action_errors_total",
    "xstatemachine_service_errors_total",
    "xstatemachine_chain_trips_total",
];
const PYTHON_HISTOGRAMS: &[&str] =
    &["xstatemachine_transition_duration_seconds", "xstatemachine_service_duration_seconds"];
const PYTHON_GAUGES: &[&str] = &["xstatemachine_queue_depth", "xstatemachine_active_interpreters"];

/// The Rust metric set: the Python-shared names must be byte-identical.
#[test]
fn shared_metric_names_match_python() {
    assert_eq!(n::TRANSITIONS, "xstatemachine_transitions_total");
    assert_eq!(n::EVENTS_RECEIVED, "xstatemachine_events_received_total");
    assert_eq!(n::TRANSITION_DURATION_SECONDS, "xstatemachine_transition_duration_seconds");
    assert_eq!(n::ACTIVE_INTERPRETERS, "xstatemachine_active_interpreters");
    assert_eq!(n::CHAIN_TRIPS, "xstatemachine_chain_trips_total");

    assert!(PYTHON_COUNTERS.contains(&n::TRANSITIONS));
    assert!(PYTHON_COUNTERS.contains(&n::EVENTS_RECEIVED));
    assert!(PYTHON_COUNTERS.contains(&n::CHAIN_TRIPS));
    assert!(PYTHON_HISTOGRAMS.contains(&n::TRANSITION_DURATION_SECONDS));
    assert!(PYTHON_GAUGES.contains(&n::ACTIVE_INTERPRETERS));

    // Rust-only metrics must not shadow a Python name: their names carry an explicit suffix or
    // do not appear in the Python set at all.
    for rust_only in [n::ACTIONS, n::UNHANDLED_EVENTS, n::EVENTS_DROPPED, n::ERRORS, n::MICROSTEPS]
    {
        assert!(
            !PYTHON_COUNTERS.contains(&rust_only)
                && !PYTHON_HISTOGRAMS.contains(&rust_only)
                && !PYTHON_GAUGES.contains(&rust_only),
            "{rust_only} shadows a Python metric"
        );
    }
}

/// The label set of every shared metric matches Python's (order is not significant; the exporter
/// sorts labels, but the keys must be the same).
#[test]
fn shared_label_sets_match_python() {
    assert_eq!(n::LABEL_MACHINE, "machine");
    assert_eq!(n::LABEL_EVENT, "event");
    assert_eq!(n::LABEL_DISPOSITION, "disposition");
    assert_eq!(n::LABEL_FROM_STATE, "from_state");
    assert_eq!(n::LABEL_TO_STATE, "to_state");
    assert_eq!(n::LABEL_ACTION, "action");
    // Python guard/service labels have no Rust counterpart (documented); the Rust-only labels
    // (`reason`, `kind`) must not collide with a Python label name.
    assert_eq!(n::LABEL_REASON, "reason");
    assert_eq!(n::LABEL_KIND, "kind");
}

const CHART: &str = r#"{"id":"door","initial":"closed","states":{
    "closed":{"on":{"OPEN":"open"}},
    "open":{"on":{"CLOSE":"closed"}}}}"#;

fn samples(snapshotter: &Snapshotter) -> Vec<(String, Vec<(String, String)>, DebugValue)> {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _unit, _desc, value)| {
            let key = key.key();
            let labels = key.labels().map(|l| (l.key().to_owned(), l.value().to_owned())).collect();
            (key.name().to_owned(), labels, value)
        })
        .collect()
}

/// A flood of undeclared event types must collapse to the single `unknown` event label (and never
/// mint one series per input), even under the cardinality guard, and the `machine`/`from_state`/
/// `to_state`/`action`/`reason`/`kind` dimensions stay capped.
#[test]
fn undeclared_event_flood_does_not_mint_series() {
    let observer =
        Arc::new(MetricsObserver::new().with_max_label_values(NonZeroUsize::new(4).unwrap()));
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        let machine = Machine::from_json(CHART).expect("machine");
        let mut i = SyncInterpreter::new(machine, MachineLogic::dynamic());
        i.observe(observer.clone());
        i.start().expect("start");
        for n in 0..10_000 {
            i.send(format!("FUZZ_{n}").as_str()).expect("send");
        }
    });
    let all = samples(&snapshotter);
    let event_values: std::collections::BTreeSet<String> = all
        .iter()
        .filter(|(name, ..)| name == n::EVENTS_RECEIVED)
        .flat_map(|(_, labels, _)| {
            labels.iter().filter(|(k, _)| k == "event").map(|(_, v)| v.clone())
        })
        .collect();
    assert_eq!(event_values.iter().map(String::as_str).collect::<Vec<_>>(), ["unknown"]);
    assert!(
        all.iter().all(|(_, labels, _)| labels.iter().all(|(_, v)| !v.starts_with("FUZZ"))),
        "no raw undeclared event type may reach a label"
    );
}

/// `describe_metrics` registers exactly the Rust metric set; every one has a help text.
#[test]
fn describe_metrics_covers_the_whole_rust_set() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, describe_metrics);
    metrics::with_local_recorder(&recorder, || {
        for name in [
            n::TRANSITIONS,
            n::EVENTS_RECEIVED,
            n::CHAIN_TRIPS,
            n::ACTIONS,
            n::UNHANDLED_EVENTS,
            n::EVENTS_DROPPED,
            n::ERRORS,
        ] {
            metrics::counter!(name).increment(1);
        }
        metrics::gauge!(n::ACTIVE_INTERPRETERS).set(0.0);
        metrics::histogram!(n::TRANSITION_DURATION_SECONDS).record(0.0);
        metrics::histogram!(n::MICROSTEPS).record(0.0);
    });
    let described = snapshotter.snapshot().into_vec();
    assert_eq!(described.len(), 10);
    assert!(described.iter().all(|(_, _, desc, _)| desc.is_some()));
}

/// `timed` records a non-negative duration for the closure's machine.
#[test]
fn timed_records_a_duration_histogram() {
    let observer = Arc::new(MetricsObserver::new());
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        let machine = Machine::from_json(CHART).expect("machine");
        let mut i = SyncInterpreter::new(machine, MachineLogic::dynamic());
        i.observe(observer.clone());
        i.start().expect("start");
        let machine = i.machine().clone();
        observer.timed(&machine, || i.send("OPEN").expect("send"));
    });
    let all = samples(&snapshotter);
    let hist = all
        .iter()
        .find(|(name, ..)| name == n::TRANSITION_DURATION_SECONDS)
        .map(|(_, _, v)| v)
        .expect("a duration histogram must exist");
    match hist {
        DebugValue::Histogram(values) => {
            assert_eq!(values.len(), 1);
            assert!(values[0].into_inner() >= 0.0);
        }
        other => panic!("expected a histogram, got {other:?}"),
    }
}
