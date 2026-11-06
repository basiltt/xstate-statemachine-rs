//! Runtime behaviour aligned with the Python library 0.12.0: snapshot timestamps, stranded
//! events, corrupt snapshot records, the simulated clock and the pure transition API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::json;
use xstate_core::{
    DynContext, ErrorKind, Event, Machine, MachineLogic, Observer, RestoreOptions, SimulatedClock,
    Snapshot, Status, SyncInterpreter, pure,
};

fn machine(text: &str) -> Machine {
    Machine::from_json(text).expect("machine")
}

fn timed() -> Machine {
    machine(
        r#"{"id":"m","initial":"a","states":{"a":{"after":{"100":"b","200":"c"},"on":{"GO":"b"}},"b":{"on":{"BACK":"a"}},"c":{}}}"#,
    )
}

/// `taken_at` is the interpreter clock's wall time, so a simulated snapshot lines up with
/// its own deadlines.
#[test]
fn snapshot_taken_at_uses_the_interpreter_clock() {
    let clock = SimulatedClock::new();
    let mut i =
        SyncInterpreter::with_clock(timed(), MachineLogic::dynamic(), Arc::new(clock.clone()));
    i.start().unwrap();
    i.advance(&clock, 40.0).unwrap();
    let snap = i.snapshot().unwrap();
    assert!((snap.taken_at - 0.04).abs() < 1e-9, "{}", snap.taken_at);
    assert!((snap.deadlines[0].due_at_wall - 0.1).abs() < 1e-9);
}

#[derive(Default)]
struct Drops(Mutex<Vec<String>>);

impl Observer<DynContext> for Drops {
    fn on_event_dropped(&self, _m: &Machine, event: &Event, reason: &'static str) {
        self.0.lock().unwrap().push(format!("{} {reason}", event.ty));
    }
}

fn snapshot_with_pending(records: &serde_json::Value) -> Snapshot {
    let mut i = SyncInterpreter::new(timed(), MachineLogic::dynamic());
    i.start().unwrap();
    let mut snap = i.snapshot().unwrap();
    snap.pending_events.clone_from(records.as_array().unwrap());
    snap
}

fn restore(snap: &Snapshot) -> xstate_core::Result<SyncInterpreter> {
    SyncInterpreter::from_snapshot(
        timed(),
        MachineLogic::dynamic(),
        snap,
        Arc::new(SimulatedClock::new()),
        &RestoreOptions::default(),
    )
}

/// Events still queued when the machine is stopped are reported, not silently discarded.
#[test]
fn events_stranded_at_stop_are_reported() {
    let snap = snapshot_with_pending(&json!([{"kind": "event", "type": "GO"}]));
    let mut i = restore(&snap).unwrap();
    let drops = Arc::new(Drops::default());
    i.observe(drops.clone());
    i.stop();
    assert_eq!(*drops.0.lock().unwrap(), ["GO stopped"]);
    assert_eq!(i.status(), Status::Stopped);
}

/// A pending-event record with an unknown `kind`, a non-object `payload` or no `type` is a
/// corrupt snapshot, not user traffic.
#[test]
fn corrupt_event_records_refuse_the_restore() {
    for bad in [
        json!({"kind": "bogus", "type": "GO"}),
        json!({"kind": "event", "type": "GO", "payload": [1]}),
        json!({"kind": "event", "type": ""}),
        json!("GO"),
        json!({"kind": "after", "type": "after.100.m.a", "fired_at": "soon"}),
    ] {
        let err = restore(&snapshot_with_pending(&json!([bad]))).expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Snapshot, "{bad}: {err}");
    }
    let ok = json!([{"type": "GO", "payload": null}, {"kind": "system", "type": "GO"},
        {"kind": "done", "engine": true, "type": "done.invoke.svc", "data": 1}]);
    let mut i = restore(&snapshot_with_pending(&ok)).unwrap();
    i.tick().unwrap();
    assert_eq!(i.state_ids(), ["m.b"]);
}

/// Only a state that declares `after` owns timers (the engine cancels timers on exit only for
/// such a state), so a deadline naming another state is a corrupt snapshot.
#[test]
fn deadline_of_a_state_without_after_refuses_the_restore() {
    let clock = SimulatedClock::new();
    let mut i =
        SyncInterpreter::with_clock(timed(), MachineLogic::dynamic(), Arc::new(clock.clone()));
    i.start().unwrap();
    let mut snap = i.snapshot().unwrap();
    assert_eq!(snap.deadlines.len(), 2);
    snap.deadlines[0].state_id = "m.b".into();
    let err = restore(&snap).expect_err("refused");
    assert_eq!(err.kind(), ErrorKind::Snapshot, "{err}");
    assert!(err.to_string().contains("m.b"), "{err}");
}

/// Each due timer is re-checked before it runs: one cancelled by an earlier timer of the
/// same batch does not fire. Non-finite clock moves are refused.
#[test]
fn simulated_clock_batches_and_rejects_non_finite_times() {
    let clock = SimulatedClock::new();
    let mut i =
        SyncInterpreter::with_clock(timed(), MachineLogic::dynamic(), Arc::new(clock.clone()));
    i.start().unwrap();
    i.advance(&clock, 300.0).unwrap();
    assert_eq!(i.state_ids(), ["m.b"], "the 200 ms timer was cancelled by leaving `a`");
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(clock.try_increment(bad).is_err());
        assert!(clock.try_set(bad).is_err());
        assert!(i.advance(&clock, bad).is_err());
    }
    clock.increment(f64::NAN);
    assert!((xstate_core::Clock::now_ms(&clock) - 300.0).abs() < f64::EPSILON);
}

/// The pure transition API returns a finished snapshot unchanged, with no actions.
#[test]
fn pure_transition_leaves_a_finished_state_alone() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"END":"z"}},"z":{"type":"final","entry":"never"}}}"#,
    );
    let logic = MachineLogic::dynamic();
    let (start, _) = pure::initial_transition(&m, &logic, &json!(null)).unwrap();
    let (done, planned) = pure::transition(&m, &logic, &start, Event::new("END")).unwrap();
    assert_eq!(done.status(), Status::Done);
    assert_eq!(planned.len(), 1);
    let (again, planned) = pure::transition(&m, &logic, &done, Event::new("END")).unwrap();
    assert_eq!(planned.len(), 0);
    assert_eq!(again.status(), Status::Done);
    assert_eq!(again.leaf_ids(&m), done.leaf_ids(&m));
}
