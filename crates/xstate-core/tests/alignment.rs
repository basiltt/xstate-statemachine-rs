//! Engine behaviour aligned with the Python library 0.12.0 (and XState v5 where it has an
//! opinion): action-error policies on nested and final-state failures, delays, unresolved
//! targets, assign on a non-object context and user actions spelled like built-ins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use xstate_core::{
    Context as _, DynContext, Error, ErrorKind, Event, Machine, MachineLogic, Observer,
    SimulatedClock, Status, SyncInterpreter,
};

fn machine(text: &str) -> Machine {
    Machine::from_json(text).expect("machine")
}

fn started(m: Machine, logic: MachineLogic<DynContext>) -> SyncInterpreter {
    let mut i = SyncInterpreter::new(m, logic);
    i.start().expect("start");
    i
}

fn failing() -> MachineLogic<DynContext> {
    MachineLogic::dynamic()
        .action("ok", |_| Ok(()))
        .action("boom", |_| Err(Error::logic("boom")))
        .action("bump", |s| s.context.assign_json(json!({"n": 1}).as_object().unwrap()))
}

/// A failure inside `choose` is a failure of the step: `rollback` undoes the transition and
/// its partial context, `fail` stops the machine naming the innermost action.
#[test]
fn choose_failures_follow_the_action_error_policy() {
    let chart = |policy: &str| {
        machine(&format!(
            r#"{{"id":"m","actionErrorPolicy":"{policy}","initial":"a","context":{{"n":0}},"states":{{
                "a":{{"on":{{"GO":{{"target":"b","actions":{{"type":"choose","params":{{"branches":[{{"actions":["bump","boom"]}}]}}}}}}}}}},
                "b":{{}}}}}}"#
        ))
    };
    let mut i = started(chart("rollback"), failing());
    let r = i.send("GO").expect("rollback keeps the machine");
    assert_eq!(i.state_ids(), ["m.a"]);
    assert_eq!(i.context().unwrap()["n"], 0, "partial context rolled back");
    assert_eq!(r.status, Status::Running);

    let mut i = started(chart("fail"), failing());
    let err = i.send("GO").unwrap_err();
    assert!(matches!(&err, Error::TransitionFailed { action, .. } if action == "boom"), "{err}");
    assert_eq!(i.status(), Status::Error);

    let mut i = started(chart("continue"), failing());
    i.send("GO").expect("continue");
    assert_eq!(i.state_ids(), ["m.b"]);
}

/// An entry action failing on a top-level final state: completion applies only when the
/// microstep commits.
#[test]
fn failing_entry_on_a_top_level_final_state_does_not_complete() {
    let chart = |policy: &str| {
        machine(&format!(
            r#"{{"id":"m","actionErrorPolicy":"{policy}","initial":"a","states":{{
                "a":{{"on":{{"END":"done"}}}},"done":{{"type":"final","entry":"boom"}}}}}}"#
        ))
    };
    let mut i = started(chart("rollback"), failing());
    i.send("END").expect("rolled back");
    assert_eq!((i.status(), i.state_ids()), (Status::Running, vec!["m.a".to_owned()]));

    let mut i = started(chart("fail"), failing());
    assert!(i.send("END").is_err());
    assert_eq!(i.status(), Status::Error);
}

/// A rollback restores the history recorded by the undone transition.
#[test]
fn rollback_restores_history() {
    let m = machine(
        r#"{"id":"m","actionErrorPolicy":"rollback","initial":"p","states":{
            "p":{"initial":"x","states":{"x":{"on":{"NEXT":"y"}},"y":{},"h":{"type":"history"}},
                 "on":{"LEAVE":{"target":"out","actions":"boom"},"OK":"out"}},
            "out":{"on":{"BACK":"p.h"}}}}"#,
    );
    let mut i = started(m, failing());
    i.send("NEXT").unwrap();
    i.send("LEAVE").unwrap();
    assert!(i.state().unwrap().history().is_empty(), "the undone exit recorded nothing");
    assert_eq!(i.state_ids(), ["m.p.y"]);
    i.send("OK").unwrap();
    i.send("BACK").unwrap();
    assert_eq!(i.state_ids(), ["m.p.y"], "history from the committed exit");
}

/// `assign` on a context that is not an object is an action error, not a silent reset.
#[test]
fn assign_on_a_non_object_context_is_an_action_error() {
    let chart = |policy: &str| {
        machine(&format!(
            r#"{{"id":"m","actionErrorPolicy":"{policy}","context":[1,2],"initial":"a","states":{{
                "a":{{"on":{{"SET":{{"actions":{{"type":"assign","params":{{"n":1}}}}}}}}}}}}}}"#
        ))
    };
    let mut i = started(chart("rollback"), MachineLogic::dynamic());
    i.send("SET").unwrap();
    assert_eq!(i.context().unwrap(), &json!([1, 2]));
    let mut i = started(chart("fail"), MachineLogic::dynamic());
    assert_eq!(i.send("SET").unwrap_err().kind(), ErrorKind::TransitionFailed);
}

/// A delay that resolves to NaN, infinity or an error disables that one timer (logged) on
/// every path; it never fails the step.
#[test]
fn unusable_delays_disable_only_their_timer() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"entry":[{"type":"raise","params":{"event":"LATE","delay":"NAN"}},
                          {"type":"sendTo","params":{"to":"self","event":"LATE","delay":"BROKEN"}}],
                 "after":{"INF":"b","BROKEN":"b","NAN":"b","OK":"c"},
                 "on":{"LATE":"b"}},
            "b":{},"c":{}}}"#,
    );
    let logic = MachineLogic::dynamic()
        .delay_fn("NAN", |_, _| Ok(f64::NAN))
        .delay_fn("INF", |_, _| Ok(f64::INFINITY))
        .delay_fn("BROKEN", |_, _| Err(Error::logic("no delay today")))
        .delay("OK", 10.0);
    let clock = SimulatedClock::new();
    let mut i = SyncInterpreter::with_clock(m, logic, Arc::new(clock.clone()));
    i.start().expect("a bad delay does not fail the start");
    assert_eq!(i.pending_timers(), 1, "only the usable timer is armed");
    i.advance(&clock, 10.0).unwrap();
    assert_eq!(i.state_ids(), ["m.c"]);
}

#[derive(Default)]
struct Drops(Mutex<Vec<String>>);

impl Observer<DynContext> for Drops {
    fn on_event_dropped(&self, _m: &Machine, event: &Event, reason: &'static str) {
        self.0.lock().unwrap().push(format!("{} {reason}", event.ty));
    }
}

/// `sendTo` with no live target, `sendParent` / `escalate` without a parent: the event is
/// dropped and reported, the step commits but carries an `UnresolvedTarget` soft error.
#[test]
fn unresolved_targets_are_reported_and_mark_the_step() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{
            "TO":{"target":"b","actions":{"type":"sendTo","params":{"to":"ghost","event":"HI"}}},
            "UP":{"actions":{"type":"sendParent","params":{"event":"HI"}}},
            "ESC":{"actions":{"type":"escalate","params":{"error":"bad"}}},
            "FWD":{"actions":{"type":"forwardTo","params":{"to":"ghost"}}}}},
            "b":{"on":{"BACK":"a"}}}}"#,
    );
    let drops = Arc::new(Drops::default());
    let mut i = SyncInterpreter::new(m, MachineLogic::dynamic());
    i.observe(drops.clone());
    i.start().unwrap();
    let r = i.send("TO").unwrap();
    assert_eq!(i.state_ids(), ["m.b"], "the transition still commits");
    let err = r.error.expect("soft error");
    assert_eq!(err.kind(), ErrorKind::UnresolvedTarget);
    assert!(err.to_string().contains("sendTo target 'ghost'"), "{err}");
    i.send("BACK").unwrap();
    for (event, action) in [("UP", "sendParent"), ("ESC", "escalate"), ("FWD", "sendTo")] {
        let err = i.send(event).unwrap().error.expect("soft error");
        assert!(err.to_string().starts_with(action), "{err}");
    }
    assert!(i.send("BACK").unwrap().error.is_none(), "a clean step carries none");
    assert_eq!(
        *drops.0.lock().unwrap(),
        [
            "HI unresolved_target",
            "HI unresolved_target",
            "xstate.error.actor.m unresolved_target",
            "FWD unresolved_target"
        ]
    );
}

/// A delayed `sendTo` addressed to the machine itself is one of its timers.
#[test]
fn delayed_send_to_self_is_a_timer() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"entry":{"type":"sendTo","params":{"to":"self","event":"GO","delay":50}},"on":{"GO":"b"}},
            "b":{}}}"#,
    );
    let clock = SimulatedClock::new();
    let mut i = SyncInterpreter::with_clock(m, MachineLogic::dynamic(), Arc::new(clock.clone()));
    i.start().unwrap();
    i.advance(&clock, 50.0).unwrap();
    assert_eq!(i.state_ids(), ["m.b"]);
}

/// A user action spelled like a built-in wins: at build time its own `params` are not held to
/// the built-in's signature, and at run time the user's implementation runs.
#[test]
fn user_actions_win_over_built_ins() {
    let ran = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = Arc::clone(&ran);
    let logic = MachineLogic::dynamic().action("cancel", move |s| {
        seen.lock().unwrap().push(s.params.clone());
        Ok(())
    });
    let chart = json!({"id":"m","initial":"a","states":{"a":{"entry":{"type":"cancel","params":{"order":7}}}}});
    assert!(Machine::from_value(&chart).is_err(), "the built-in `cancel` needs `sendId`");
    let m = Machine::from_value_with_logic(
        &chart,
        &xstate_core::config::ParseOptions::default(),
        &logic,
    )
    .expect("user wins");
    let i = started(m, logic.clone());
    assert_eq!(*ran.lock().unwrap(), [json!({"order": 7})]);
    drop(i);

    // A valid built-in spelling compiled without the logic: the user action still wins.
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"entry":{"type":"cancel","params":{"sendId":"x"}}}}}"#,
    );
    let r = started(m, logic).send("NOTHING").unwrap();
    assert!(r.unhandled);
    assert_eq!(ran.lock().unwrap().len(), 2);
}

/// Dotted machine ids and dotted custom ids resolve, including `#my.id.child`.
#[test]
fn dotted_ids_resolve() {
    let m = machine(
        r##"{"id":"my.machine","initial":"a","states":{
            "a":{"on":{"GO":"#my.id.child","DEEP":"#my.machine.v1.0"}},
            "p":{"id":"my.id","initial":"child","states":{"child":{}}},
            "v1":{"initial":"0","states":{"0":{}}}}}"##,
    );
    assert_eq!(m.state_by_id("my.id.child"), m.state_by_id("my.machine.p.child"));
    assert!(m.state_by_id("my.machine.v1.0").is_some());
    let mut i = started(m, MachineLogic::dynamic());
    i.send("GO").unwrap();
    assert_eq!(i.state_ids(), ["my.machine.p.child"]);
}

#[test]
fn an_empty_event_type_is_invalid() {
    let mut i =
        started(machine(r#"{"id":"m","initial":"a","states":{"a":{}}}"#), MachineLogic::dynamic());
    assert_eq!(i.send("").unwrap_err().kind(), ErrorKind::InvalidEvent);
    assert_eq!(Event::from_json(json!("")).unwrap_err().kind(), ErrorKind::InvalidEvent);
    assert_eq!(Event::from_json(json!({"type": ""})).unwrap_err().kind(), ErrorKind::InvalidEvent);
}

/// `after` keys are normalised (`"1e3"` is 1000 ms and names `xstate.after.1000.<id>`); two
/// keys naming the same delay, and fractional or negative ones, are refused.
#[test]
fn after_keys_are_normalised_and_validated() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"after":{"1e3":"b"," 100":"b","+5":"b"}},"b":{}}}"#,
    );
    let keys: Vec<(&str, &str)> =
        m.states()[1].after.iter().map(|a| (a.key.as_str(), a.event.as_str())).collect();
    assert_eq!(
        keys,
        [
            ("1000", "xstate.after.1000.m.a"),
            ("100", "xstate.after.100.m.a"),
            ("5", "xstate.after.5.m.a")
        ]
    );
    for bad in [r#""1.5""#, r#""-5""#, r#""""#, r#"" ""#] {
        let text = format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"after":{{{bad}:"b"}}}},"b":{{}}}}}}"#
        );
        let err = Machine::from_json(&text).unwrap_err().to_string();
        assert!(err.contains("is not a valid delay"), "{bad}: {err}");
    }
    let err = Machine::from_json(
        r#"{"id":"m","initial":"a","states":{"a":{"after":{"100":"b"," 100":"b"}},"b":{}}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("both name the delay '100'"), "{err}");
    // A name that only looks numeric to `f64::from_str` stays a name.
    let m = machine(r#"{"id":"m","initial":"a","states":{"a":{"after":{"inf":"b"}},"b":{}}}"#);
    assert_eq!(m.states()[1].after[0].delay, xstate_core::machine::Delay::Named("inf".into()));
}
