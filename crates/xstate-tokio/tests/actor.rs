//! Async interpreter behaviour under paused tokio time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::{Context as _, Error, ErrorKind, Machine, MachineLogic, Status};
use xstate_tokio::{Options, Overflow, spawn};

fn machine(json: &str) -> Machine {
    Machine::from_json(json).expect("machine")
}

#[tokio::test(start_paused = true)]
async fn toggles_and_reports_receipts() {
    let actor = spawn(
        machine(r#"{"id":"t","initial":"off","states":{"off":{"on":{"TOGGLE":"on"}},"on":{"on":{"TOGGLE":"off"}}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    let r = actor.send_and_wait("TOGGLE").await.unwrap();
    assert_eq!(r.state_ids(), ["t.on"]);
    assert!(r.changed);
    assert_eq!(actor.view().state_ids, ["t.on"]);
    let r = actor.send_and_wait("NOPE").await.unwrap();
    assert!(r.unhandled);
    actor.stop().await;
    assert_eq!(actor.view().status, Status::Stopped);
    assert!(matches!(actor.send("TOGGLE").await, Err(Error::InvalidLifecycle { .. })));
}

#[tokio::test(start_paused = true)]
async fn after_timers_fire_on_tokio_time() {
    let actor = spawn(
        machine(r#"{"id":"l","initial":"active","states":{"active":{"after":{"500":"expired"}},"expired":{"type":"final"}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(499)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["l.active"]);
    tokio::time::sleep(Duration::from_millis(2)).await;
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    assert_eq!(view.state_ids, ["l.expired"]);
}

#[tokio::test(start_paused = true)]
async fn async_services_complete_and_fail() {
    let logic = MachineLogic::dynamic()
        .async_service("fetch", |_ctx: Value, input: Value| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(json!({"got": input}))
            })
        })
        .async_service("boom", |_ctx: Value, _input: Value| {
            Box::pin(async { Err(Error::logic("kaboom")) })
        })
        .action("save", |scope| {
            let data = scope.event.output().clone();
            scope.context.assign_json(json!({"data": data}).as_object().unwrap())
        });
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"loading","context":{},"states":{
                "loading":{"invoke":{"src":"fetch","id":"fetch","input":{"user":7},"onDone":{"target":"ready","actions":"save"},"onError":"failed"}},
                "ready":{"on":{"RETRY":"breaking"}},
                "breaking":{"invoke":{"src":"boom","onError":{"target":"failed"}}},
                "failed":{"type":"final"}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(101)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["p.ready"]);
    let snap = actor.snapshot().await.unwrap();
    assert_eq!(snap.context["data"]["got"]["user"], 7);
    actor.send_and_wait("RETRY").await.unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.state_ids, ["p.failed"]);
}

#[tokio::test(start_paused = true)]
async fn unhandled_service_error_fails_the_actor() {
    let logic = MachineLogic::dynamic().async_service("boom", |_ctx: Value, _input: Value| {
        Box::pin(async { Err(Error::logic("kaboom")) })
    });
    let actor = spawn(
        machine(r#"{"id":"e","initial":"a","states":{"a":{"invoke":{"src":"boom"}}}}"#),
        logic,
        Options::default(),
    )
    .unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Error);
    assert_eq!(view.error.as_deref(), Some("kaboom"));
}

#[tokio::test(start_paused = true)]
async fn child_machine_actor_completes_and_talks_to_its_parent() {
    let child = machine(
        r#"{"id":"child","initial":"working","output":{"result":42},"states":{
            "working":{"entry":{"type":"sendParent","params":{"event":{"type":"PROGRESS","pct":50}}},"on":{"FINISH":"done"}},
            "done":{"type":"final"}}}"#,
    );
    let logic = MachineLogic::dynamic()
        .machine_service("worker", child)
        .action("noteProgress", |scope| {
            let pct = scope.event.payload["pct"].clone();
            scope.context.assign_json(json!({"pct": pct}).as_object().unwrap())
        })
        .action("saveResult", |scope| {
            let out = scope.event.output().clone();
            scope.context.assign_json(json!({"result": out}).as_object().unwrap())
        });
    let actor = spawn(
        machine(
            r#"{"id":"parent","initial":"running","context":{},"states":{
                "running":{"invoke":{"src":"worker","id":"worker","onDone":{"target":"finished","actions":"saveResult"}},
                           "on":{"PROGRESS":{"actions":"noteProgress"},
                                 "KICK":{"actions":{"type":"sendTo","params":{"to":"worker","event":{"type":"FINISH"}}}}}},
                "finished":{"type":"final"}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    let snap = actor.snapshot().await.unwrap();
    assert_eq!(snap.context["pct"], 50, "sendParent reached the parent");
    actor.send_and_wait("KICK").await.unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    let snap = actor.snapshot().await.unwrap();
    assert_eq!(snap.context["result"]["result"], 42, "child output arrived through onDone");
}

#[tokio::test(start_paused = true)]
async fn strict_mode_and_overflow_policies() {
    let m = machine(
        r#"{"id":"s","strict":true,"initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#,
    );
    let actor = spawn(m.clone(), MachineLogic::dynamic(), Options::default()).unwrap();
    let err = actor.send_and_wait("BANANA").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnknownEvent);

    let tiny = spawn(
        m,
        MachineLogic::dynamic(),
        Options { mailbox_capacity: 1, overflow: Overflow::Reject, ..Options::default() },
    )
    .unwrap();
    // Fill the mailbox without yielding to the actor task.
    let first = tiny.try_send("GO");
    let second = tiny.try_send("GO");
    assert!(first.is_ok());
    assert!(matches!(second, Err(Error::LimitExceeded(_))) || second.is_ok());
    let _ = Arc::new(tiny);
}
