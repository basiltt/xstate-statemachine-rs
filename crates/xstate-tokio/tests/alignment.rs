//! Async interpreter behaviour aligned with the Python library 0.12.0: exit actions reach the
//! state's own invoke before it is stopped, unresolved targets, empty event types, delayed
//! self-sends and `subscribe` notifications for targetless transitions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use xstate_core::{
    Context as _, DynContext, ErrorKind, Event, Machine, MachineLogic, Observer, Status,
};
use xstate_tokio::{Options, spawn, spawn_with_observers};

fn machine(json: &str) -> Machine {
    Machine::from_json(json).expect("machine")
}

/// XState `exitStates`: a state's exit actions run before its invoked actors are stopped, so
/// an exit action can still `sendTo` its own invoke.
#[tokio::test(start_paused = true)]
async fn exit_action_reaches_the_states_own_invoke() {
    let child = machine(
        r#"{"id":"child","initial":"idle","states":{"idle":{"on":{"PING":{"actions":"gotPing"}}}}}"#,
    );
    let pinged = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&pinged);
    let logic =
        MachineLogic::dynamic().machine_service("worker", child).action("gotPing", move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
    let parent = machine(
        r#"{"id":"p","initial":"busy","states":{
            "busy":{"invoke":{"src":"worker","id":"worker"},
                    "exit":{"type":"sendTo","params":{"to":"worker","event":"PING"}},
                    "on":{"LEAVE":"idle"}},
            "idle":{}}}"#,
    );
    let actor = spawn(parent, logic, Options::default()).unwrap();
    tokio::task::yield_now().await;
    let r = actor.send_and_wait("LEAVE").await.unwrap();
    assert!(r.error.is_none(), "the invoke was still live: {:?}", r.error);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(pinged.load(Ordering::SeqCst), "the child got PING before it was stopped");
}

#[derive(Default)]
struct Drops(Mutex<Vec<String>>);

impl Observer<DynContext> for Drops {
    fn on_event_dropped(&self, _m: &Machine, event: &Event, reason: &'static str) {
        self.0.lock().unwrap().push(format!("{} {reason}", event.ty));
    }
}

#[tokio::test(start_paused = true)]
async fn unresolved_targets_are_reported_and_mark_the_step() {
    let m = machine(
        r#"{"id":"r","initial":"a","states":{"a":{"on":{
            "TO":{"actions":{"type":"sendTo","params":{"to":"ghost","event":"HI"}}},
            "LATER":{"actions":{"type":"sendTo","params":{"to":"ghost","event":"HI","delay":10}}},
            "UP":{"actions":{"type":"sendParent","params":{"event":"HI"}}},
            "ESC":{"actions":{"type":"escalate","params":{"error":"bad"}}},
            "OK":{"actions":"ok"}}}}}"#,
    );
    let drops = Arc::new(Drops::default());
    let logic = MachineLogic::dynamic().action("ok", |_| Ok(()));
    let actor = spawn_with_observers(m, logic, Options::default(), vec![drops.clone()]).unwrap();
    for (event, action) in
        [("TO", "sendTo"), ("LATER", "sendTo"), ("UP", "sendParent"), ("ESC", "escalate")]
    {
        let err = actor.send_and_wait(event).await.unwrap().error.expect("soft error");
        assert_eq!(err.kind(), ErrorKind::UnresolvedTarget);
        assert!(err.to_string().starts_with(action), "{err}");
    }
    assert!(actor.send_and_wait("OK").await.unwrap().error.is_none());
    assert_eq!(drops.0.lock().unwrap().len(), 4);
    assert!(drops.0.lock().unwrap().iter().all(|d| d.ends_with("unresolved_target")));
}

#[tokio::test(start_paused = true)]
async fn empty_event_type_is_invalid() {
    let actor = spawn(
        machine(r#"{"id":"e","initial":"a","states":{"a":{}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    assert_eq!(actor.send_and_wait("").await.unwrap_err().kind(), ErrorKind::InvalidEvent);
}

#[tokio::test(start_paused = true)]
async fn delayed_send_to_self_fires_and_can_be_cancelled() {
    let m = machine(
        r#"{"id":"d","initial":"a","states":{
            "a":{"entry":[{"type":"sendTo","params":{"to":"self","event":"GO","delay":50}},
                          {"type":"sendTo","params":{"to":"self","event":"NEVER","delay":20,"id":"n"}},
                          {"type":"cancel","params":{"sendId":"n"}}],
                 "on":{"GO":"b","NEVER":"z"}},
            "b":{},"z":{}}}"#,
    );
    let actor = spawn(m, MachineLogic::dynamic(), Options::default()).unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["d.b"]);
}

/// `subscribe()` is notified for a targetless (actions-only) transition too.
#[tokio::test(start_paused = true)]
async fn subscribers_see_targetless_transitions() {
    let m = machine(
        r#"{"id":"c","initial":"a","context":{"n":0},"states":{"a":{"on":{"INC":{"actions":"inc"}}}}}"#,
    );
    let logic = MachineLogic::dynamic().action("inc", |s| {
        let n = s.context["n"].as_i64().unwrap_or(0) + 1;
        s.context.assign_json(json!({"n": n}).as_object().unwrap())
    });
    let actor = spawn(m, logic, Options::default()).unwrap();
    tokio::task::yield_now().await;
    let mut rx = actor.subscribe();
    rx.borrow_and_update();
    actor.send_and_wait("INC").await.unwrap();
    assert!(rx.has_changed().unwrap(), "a targetless step notifies subscribers");
    assert_eq!(actor.snapshot().await.unwrap().context["n"], 1);
    assert_eq!(actor.view().status, Status::Running);
}
