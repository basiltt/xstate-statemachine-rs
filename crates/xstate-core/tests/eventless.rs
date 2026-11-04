//! Eventless (`always`) selection follows XState v5's `macrostep` and
//! `selectEventlessTransitions` (verified against xstate 5.33.3; the conformance cases
//! `046`, `048` and `050` pin the live behaviour, these tests the parts no case covers).

use std::sync::{Arc, Mutex};

use serde_json::json;
use xstate_core::{
    DynContext, Machine, MachineLogic, RestoreOptions, SimulatedClock, SyncInterpreter,
};

/// Regions r1 (`a` -E-> `b`) and r2 (`c` -F-> `d`); `b` and `d` carry an `always` guarded by
/// `gB` / `gD`, both false here, so the eventless pass only records the guard order.
const REGIONS: &str = r##"{"id":"m","initial":"p","states":{"p":{"type":"parallel","states":{
    "r1":{"initial":"a","states":{"a":{"on":{"E":"b"}},"b":{"always":{"target":"#m.x","guard":"gB"}}}},
    "r2":{"initial":"c","states":{"c":{"on":{"F":"d"}},"d":{"always":{"target":"#m.x","guard":"gD"}}}}}},
    "x":{}}}"##;

fn recording_logic(log: &Arc<Mutex<Vec<String>>>) -> MachineLogic<DynContext> {
    ["gB", "gD"].into_iter().fold(MachineLogic::dynamic(), |logic, name| {
        let log = Arc::clone(log);
        logic.guard(name, move |_, _, _| {
            log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(name.to_owned());
            Ok(false)
        })
    })
}

fn drain(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

#[test]
fn leaves_are_visited_in_entry_order_and_a_restore_resets_it() {
    let machine = Machine::from_json(REGIONS).unwrap_or_else(|e| panic!("{e}"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut live = SyncInterpreter::new(machine.clone(), recording_logic(&log));
    live.start().unwrap_or_else(|e| panic!("{e}"));
    for event in ["F", "E", "X"] {
        live.send(event).unwrap_or_else(|e| panic!("{e}"));
    }
    // `d` was entered before `b`: XState visits it first.
    assert_eq!(drain(&log)[..], ["gD", "gD", "gB", "gD", "gB"]);

    // `restoreSnapshot` rebuilds the state list from the state value: document order.
    let snapshot = live.snapshot().unwrap_or_else(|e| panic!("{e}"));
    let mut restored = SyncInterpreter::from_snapshot(
        machine,
        recording_logic(&log),
        &snapshot,
        Arc::new(SimulatedClock::new()),
        &RestoreOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    drain(&log);
    restored.send("X").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(drain(&log)[..], ["gB", "gD"]);
}

#[test]
fn a_user_action_changing_the_context_keeps_a_targetless_always_selected() {
    // The in-place analogue of an XState `assign`: the snapshot changes, so the macrostep
    // selects the `always` again until its guard fails.
    let machine = Machine::from_json(
        r#"{"id":"m","initial":"a","context":{"n":0},
            "states":{"a":{"always":{"guard":"below3","actions":"inc"}}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let logic = MachineLogic::dynamic()
        .guard("below3", |ctx, _, _| Ok(ctx["n"].as_i64().unwrap_or(0) < 3))
        .action("inc", |scope| {
            let n = scope.context["n"].as_i64().unwrap_or(0);
            scope.context["n"] = json!(n + 1);
            Ok(())
        });
    let mut interp = SyncInterpreter::new(machine, logic);
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.context().map(|c| c["n"].clone()), Some(json!(3)));
}

#[test]
fn an_action_that_changes_nothing_runs_once_per_macrostep() {
    let machine = Machine::from_json(
        r#"{"id":"m","initial":"a","context":{"n":0},
            "states":{"a":{"always":{"actions":"noop"}}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let runs = Arc::new(Mutex::new(0_u32));
    let counter = Arc::clone(&runs);
    let logic = MachineLogic::dynamic().action("noop", move |_| {
        *counter.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Ok(())
    });
    let mut interp = SyncInterpreter::new(machine, logic);
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    interp.send("X").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(*runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 2);
}
