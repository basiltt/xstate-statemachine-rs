//! `MetricsObserver` end to end: a small machine on `SyncInterpreter`, recorded by a
//! `DebuggingRecorder` installed for the current thread.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::NonZeroUsize;
use std::sync::Arc;

use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use xstate_core::{Machine, MachineLogic, SyncInterpreter};
use xstate_observability::{MetricsObserver, describe_metrics, metric_names as n};

type Sample = (String, Vec<(String, String)>, DebugValue);

const CHART: &str = r#"{"id":"door","initial":"closed","onUnhandled":"defer","states":{
    "closed":{"on":{"OPEN":{"target":"open","actions":"creak"},"PING":{"actions":"creak"}}},
    "open":{"on":{"CLOSE":"closed","FINISH":"done"}},
    "done":{"type":"final"}}}"#;

fn interpreter(observer: &Arc<MetricsObserver>) -> SyncInterpreter {
    let machine = Machine::from_json(CHART).expect("machine");
    let mut i = SyncInterpreter::new(machine, MachineLogic::dynamic().action("creak", |_| Ok(())));
    i.observe(observer.clone());
    i
}

fn samples(snapshotter: &Snapshotter) -> Vec<Sample> {
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

fn find<'a>(all: &'a [Sample], name: &str, want: &[(&str, &str)]) -> Option<&'a DebugValue> {
    all.iter()
        .find(|(n, labels, _)| {
            n == name
                && labels.len() == want.len()
                && want.iter().all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
        })
        .map(|(_, _, v)| v)
}

fn counter(all: &[Sample], name: &str, want: &[(&str, &str)]) -> u64 {
    match find(all, name, want) {
        Some(DebugValue::Counter(v)) => *v,
        _ => 0,
    }
}

fn gauge(all: &[Sample], name: &str, want: &[(&str, &str)]) -> Option<f64> {
    match find(all, name, want) {
        Some(DebugValue::Gauge(v)) => Some(v.into_inner()),
        _ => None,
    }
}

fn run(observer: &Arc<MetricsObserver>, drive: impl FnOnce(&mut SyncInterpreter)) -> Vec<Sample> {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        let mut i = interpreter(observer);
        i.start().expect("start");
        drive(&mut i);
    });
    samples(&snapshotter)
}

const M: (&str, &str) = ("machine", "door");

#[test]
fn counts_events_transitions_actions_and_microsteps() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        i.send("OPEN").expect("send");
        i.send("CLOSE").expect("send");
    });

    let ev = |event, disposition| {
        counter(&all, n::EVENTS_RECEIVED, &[M, ("event", event), ("disposition", disposition)])
    };
    assert_eq!(ev("OPEN", "handled"), 1);
    assert_eq!(ev("CLOSE", "handled"), 1);

    let tr = |from, to, event| {
        counter(
            &all,
            n::TRANSITIONS,
            &[M, ("from_state", from), ("to_state", to), ("event", event)],
        )
    };
    assert_eq!(tr("door.closed", "door.open", "OPEN"), 1);
    assert_eq!(tr("door.open", "door.closed", "CLOSE"), 1);

    assert_eq!(counter(&all, n::ACTIONS, &[M, ("action", "creak")]), 1);
    assert!(
        matches!(find(&all, n::MICROSTEPS, &[M]), Some(DebugValue::Histogram(h)) if h.len() >= 3)
    );
    assert_eq!(counter(&all, n::UNHANDLED_EVENTS, &[M, ("event", "OPEN")]), 0);
}

#[test]
fn targetless_transition_uses_its_source_as_target() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        i.send("PING").expect("send");
    });
    let labels = [M, ("from_state", "door.closed"), ("to_state", "door.closed"), ("event", "PING")];
    assert_eq!(counter(&all, n::TRANSITIONS, &labels), 1);
}

#[test]
fn undeclared_events_collapse_to_unknown_and_count_as_unhandled() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        for n in 0..5 {
            i.send(format!("FUZZ_{n}").as_str()).expect("send");
        }
    });
    assert_eq!(
        counter(&all, n::EVENTS_RECEIVED, &[M, ("event", "unknown"), ("disposition", "unhandled")]),
        5
    );
    assert_eq!(counter(&all, n::UNHANDLED_EVENTS, &[M, ("event", "unknown")]), 5);
    assert!(
        all.iter().all(|(_, labels, _)| labels.iter().all(|(_, v)| !v.starts_with("FUZZ"))),
        "no raw undeclared event type may reach a label"
    );
    // `defer` parks each unhandled external event.
    assert_eq!(counter(&all, n::EVENTS_DROPPED, &[M, ("reason", "deferred")]), 5);
}

#[test]
fn label_values_beyond_the_cap_collapse_to_other() {
    let observer =
        Arc::new(MetricsObserver::new().with_max_label_values(NonZeroUsize::new(2).unwrap()));
    assert_eq!(observer.max_label_values(), 2);
    let all = run(&observer, |i| {
        i.send("OPEN").expect("send");
        i.send("CLOSE").expect("send");
        i.send("FINISH").expect("send"); // third distinct declared event, `closed` has no FINISH
    });
    // Event dimension: OPEN and CLOSE take the two slots; FINISH overflows.
    let ev = |event| {
        counter(
            &all,
            n::EVENTS_RECEIVED,
            &[
                M,
                ("event", event),
                ("disposition", if event == "other" { "unhandled" } else { "handled" }),
            ],
        )
    };
    assert_eq!(ev("OPEN"), 1);
    assert_eq!(ev("CLOSE"), 1);
    assert_eq!(ev("other"), 1);
    // State dimensions overflow too: `door.closed`, `door.open` fill from_state; no raw id leaks.
    assert!(
        all.iter().filter(|(name, ..)| name == n::TRANSITIONS).all(|(_, labels, _)| {
            labels.iter().filter(|(k, _)| k == "from_state").count() == 1
        })
    );
}

#[test]
fn dropped_events_are_counted_by_reason() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        i.send("OPEN").expect("send");
        i.send("FINISH").expect("send"); // reaches the final state: machine done
        i.send("CLOSE").expect("send"); // not running any more
    });
    assert_eq!(counter(&all, n::EVENTS_DROPPED, &[M, ("reason", "not_running")]), 1);
    assert_eq!(
        counter(&all, n::EVENTS_RECEIVED, &[M, ("event", "CLOSE"), ("disposition", "dropped")]),
        1
    );
}

#[test]
fn active_interpreters_gauge_follows_start_done_and_stop() {
    let observer = Arc::new(MetricsObserver::new());
    let started = run(&observer, |_| {});
    assert_eq!(gauge(&started, n::ACTIVE_INTERPRETERS, &[M]), Some(1.0));

    let observer = Arc::new(MetricsObserver::new());
    let finished = run(&observer, |i| {
        i.send("OPEN").expect("send");
        i.send("FINISH").expect("send");
        i.stop(); // already done: must not decrement twice
    });
    assert_eq!(gauge(&finished, n::ACTIVE_INTERPRETERS, &[M]), Some(0.0));

    let observer = Arc::new(MetricsObserver::new());
    let stopped = run(&observer, SyncInterpreter::stop);
    assert_eq!(gauge(&stopped, n::ACTIVE_INTERPRETERS, &[M]), Some(0.0));
}

#[test]
fn machine_label_can_be_dropped() {
    let observer = Arc::new(MetricsObserver::new().with_machine_label(false));
    let all = run(&observer, |i| {
        i.send("OPEN").expect("send");
    });
    assert_eq!(
        counter(&all, n::EVENTS_RECEIVED, &[("event", "OPEN"), ("disposition", "handled")]),
        1
    );
}

#[test]
fn timed_records_the_transition_duration_histogram() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        let machine = i.machine().clone();
        let receipt = observer.timed(&machine, || i.send("OPEN").expect("send"));
        assert!(receipt.changed);
    });
    match find(&all, n::TRANSITION_DURATION_SECONDS, &[M]) {
        Some(DebugValue::Histogram(values)) => {
            assert_eq!(values.len(), 1);
            assert!(values[0].into_inner() >= 0.0);
        }
        other => panic!("expected a duration histogram, got {other:?}"),
    }
}

#[test]
fn payloads_and_context_never_reach_labels() {
    let observer = Arc::new(MetricsObserver::new());
    let all = run(&observer, |i| {
        let event =
            xstate_core::Event::with_payload("OPEN", serde_json::json!({"secret": "hunter2"}));
        i.send(event).expect("send");
    });
    assert!(all.iter().all(|(_, labels, _)| labels.iter().all(|(_, v)| !v.contains("hunter2"))));
}

#[test]
fn describe_metrics_registers_help_for_every_metric() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, describe_metrics);
    // Descriptions only surface once a series exists: touch each metric on the same recorder.
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
