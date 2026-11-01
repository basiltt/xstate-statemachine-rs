//! `onUnhandled` policies, strict mode and observers on the synchronous interpreter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use xstate_core::engine::{ActiveState, StepReport};
use xstate_core::{
    DynContext, Error, ErrorKind, Event, Machine, MachineLogic, Observer, Status, SyncInterpreter,
};

fn machine(on_unhandled: &str, strict: bool) -> Machine {
    Machine::from_json(&format!(
        r#"{{"id":"m","initial":"a","onUnhandled":"{on_unhandled}","strict":{strict},"states":{{
            "a":{{"on":{{"GO":"b"}}}},
            "b":{{"on":{{"LATER":{{"target":"c","actions":"later"}},"BACK":"a"}}}},
            "c":{{}}}}}}"#
    ))
    .expect("machine")
}

fn interp(m: Machine) -> SyncInterpreter {
    let mut i = SyncInterpreter::new(m, MachineLogic::dynamic().action("later", |_| Ok(())));
    i.start().expect("start");
    i
}

#[test]
fn ignore_drops_unhandled_events() {
    let mut i = interp(machine("ignore", false));
    let r = i.send("LATER").expect("send");
    assert!(r.unhandled);
    assert_eq!(i.state_ids(), ["m.a"]);
    assert_eq!(i.deferred_events().count(), 0);
}

#[test]
fn defer_replays_after_the_configuration_changes() {
    let mut i = interp(machine("defer", false));
    // LATER is not handled in `a`: parked.
    let r = i.send("LATER").expect("send");
    assert!(r.unhandled);
    assert_eq!(i.deferred_events().count(), 1);
    // GO moves to `b`, where LATER is handled: the parked event replays within the same send.
    let r = i.send("GO").expect("send");
    assert_eq!(i.state_ids(), ["m.c"]);
    assert_eq!(r.action_names(), ["later"]);
    assert_eq!(i.deferred_events().count(), 0);
}

#[test]
fn defer_round_trips_through_snapshots() {
    let mut i = interp(machine("defer", false));
    i.send("LATER").expect("send");
    let snap = i.snapshot().expect("snapshot");
    assert_eq!(snap.deferred.len(), 1);
    let restored = SyncInterpreter::<DynContext>::from_snapshot(
        i.machine().clone(),
        MachineLogic::dynamic().action("later", |_| Ok(())),
        &snap,
        Arc::new(xstate_core::SimulatedClock::new()),
        &xstate_core::RestoreOptions::default(),
    )
    .expect("restore");
    assert_eq!(restored.deferred_events().count(), 1);
}

#[test]
fn error_policy_fails_the_machine() {
    let mut i = interp(machine("error", false));
    let r = i.send("LATER").expect("send");
    assert!(r.unhandled);
    assert_eq!(i.status(), Status::Error);
    assert!(i.state().unwrap().error().unwrap().contains("LATER"));
    // A failed machine ignores everything afterwards.
    let r = i.send("GO").expect("send");
    assert!(r.unhandled);
}

#[test]
fn strict_mode_rejects_undeclared_events_before_processing() {
    let mut i = interp(machine("ignore", true));
    let err = i.send("BANANA").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnknownEvent);
    // Declared-but-not-handled-here events are still just unhandled.
    assert!(i.send("LATER").expect("send").unhandled);
    assert_eq!(i.status(), Status::Running);
}

#[derive(Default)]
struct Recording {
    lines: Mutex<Vec<String>>,
}

impl Observer<DynContext> for Recording {
    fn on_start(&self, m: &Machine, state: &ActiveState<DynContext>, _r: &StepReport) {
        self.lines.lock().unwrap().push(format!("start {:?}", state.leaf_ids(m)));
    }
    fn on_event(
        &self,
        m: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<DynContext>,
    ) {
        self.lines.lock().unwrap().push(format!(
            "{} -> {:?} (unhandled={})",
            event.ty,
            state.leaf_ids(m),
            report.unhandled
        ));
    }
    fn on_event_dropped(&self, _m: &Machine, event: &Event, reason: &'static str) {
        self.lines.lock().unwrap().push(format!("dropped {} {reason}", event.ty));
    }
    fn on_error(&self, _m: &Machine, error: &Error) {
        self.lines.lock().unwrap().push(format!("error {}", error.kind()));
    }
    fn on_stop(&self, _m: &Machine, state: &ActiveState<DynContext>) {
        self.lines.lock().unwrap().push(format!("stop {}", state.status().as_str()));
    }
}

#[test]
fn observers_see_the_lifecycle() {
    let rec = Arc::new(Recording::default());
    let mut i = SyncInterpreter::new(
        machine("defer", false),
        MachineLogic::dynamic().action("later", |_| Ok(())),
    );
    i.observe(rec.clone());
    i.start().expect("start");
    i.send("LATER").expect("send");
    i.send("GO").expect("send");
    i.stop();
    let lines = rec.lines.lock().unwrap().clone();
    assert_eq!(
        lines,
        [
            "start [\"m.a\"]",
            "LATER -> [\"m.a\"] (unhandled=true)",
            "dropped LATER deferred",
            "GO -> [\"m.b\"] (unhandled=false)",
            "LATER -> [\"m.c\"] (unhandled=false)",
            "stop stopped",
        ]
    );
}

#[test]
fn failing_action_errors_the_machine_by_default() {
    let m = Machine::from_json(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":{"target":"b","actions":"boom"}}},"b":{}}}"#,
    )
    .unwrap();
    let mut i = SyncInterpreter::new(
        m,
        MachineLogic::dynamic().action("boom", |_| Err(Error::logic("nope"))),
    );
    i.start().unwrap();
    let err = i.send("GO").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TransitionFailed);
    assert_eq!(i.status(), Status::Error);
}

#[test]
fn rollback_policy_restores_the_previous_state() {
    let m = Machine::from_json(
        r#"{"id":"m","initial":"a","actionErrorPolicy":"rollback","context":{"n":0},"states":{
            "a":{"on":{"GO":{"target":"b","actions":[{"type":"assign","params":{"assignment":{"n":1}}},"boom"]}}},"b":{}}}"#,
    )
    .unwrap();
    let mut i = SyncInterpreter::new(
        m,
        MachineLogic::dynamic().action("boom", |_| Err(Error::logic("nope"))),
    );
    i.start().unwrap();
    let r = i.send("GO").expect("rolled back, not an error");
    assert!(!r.changed);
    assert_eq!(i.state_ids(), ["m.a"]);
    assert_eq!(i.context().unwrap()["n"], 0);
    assert_eq!(i.status(), Status::Running);
}
