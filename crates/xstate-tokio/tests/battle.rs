//! Adversarial battle tests for the tokio interpreter (0.1.0 pre-release pass).
//!
//! Covers: timers under paused time (cancellation, guarded `after`, exited-state `after`),
//! mailbox overflow policies and backpressure, receipts on finished/stopped actors, child
//! machine lifecycles (`done.invoke`/`error.actor`, `stop`, panics), `System` registry
//! collisions and deregistration, panics under a multi-thread runtime, and shutdown drain.
//!
//! Everything here is test-only; it never weakens an existing test and never touches `src/`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xstate_core::{DynContext, Error, ErrorKind, Event, Machine, MachineLogic, Observer, Status};
use xstate_tokio::{Options, Overflow, spawn, spawn_with_observers};

fn machine(json: &str) -> Machine {
    Machine::from_json(json).expect("machine")
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// An `after` on a state that is exited before the deadline must not fire.
#[tokio::test(start_paused = true)]
async fn after_on_exited_state_is_cancelled() {
    let actor = spawn(
        machine(
            r#"{"id":"t","initial":"a","states":{
                "a":{"after":{"100":"expired"},"on":{"LEAVE":"c"}},
                "c":{},"expired":{}}}"#,
        ),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("LEAVE").await.unwrap();
    assert_eq!(actor.view().state_ids, ["t.c"]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["t.c"], "the exited state's `after` fired anyway");
}

/// A guarded `after`: the timer fires once, the guard refuses, the state stays put. The
/// refusal does not re-arm the timer, so the transition is lost.
#[tokio::test(start_paused = true)]
async fn after_guard_refuses_and_does_not_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = Arc::clone(&calls);
    let logic = MachineLogic::dynamic().guard("refuse", move |_ctx: &DynContext, _e, _p| {
        calls2.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    });
    let actor = spawn(
        machine(
            r#"{"id":"g","initial":"a","states":{
                "a":{"after":{"100":{"target":"b","guard":"refuse"}}},
                "b":{}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::task::yield_now().await;
    assert!(calls.load(Ordering::SeqCst) >= 1, "the guard was evaluated at fire time");
    assert_eq!(actor.view().state_ids, ["g.a"], "guarded `after` must not transition");
    assert_eq!(actor.view().status, Status::Running);
    // The timer is consumed: advancing further does not fire it again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["g.a"]);
}

/// A delayed self-send cancelled in a later step (not its own entry step).
#[tokio::test(start_paused = true)]
async fn delayed_send_cancelled_in_later_step() {
    let actor = spawn(
        machine(
            r#"{"id":"d","initial":"a","states":{
                "a":{"entry":{"type":"sendTo","params":{"to":"self","event":"LATE","delay":50,"id":"n"}},
                     "on":{"CANCEL":{"actions":{"type":"cancel","params":{"sendId":"n"}}},
                           "LATE":"z"}},
                "z":{}}}"#,
        ),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("CANCEL").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["d.a"], "cancelled delayed send fired anyway");
}

// ---------------------------------------------------------------------------
// Mailbox overflow / backpressure
// ---------------------------------------------------------------------------

/// `Overflow::Reject`: a full mailbox makes `try_send` fail with `LimitExceeded`.
#[tokio::test(start_paused = true)]
async fn overflow_reject_fails_try_send_on_full_mailbox() {
    let m = machine(r#"{"id":"r","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#);
    let actor = spawn(
        m,
        MachineLogic::dynamic(),
        Options { mailbox_capacity: 1, overflow: Overflow::Reject, ..Options::default() },
    )
    .unwrap();
    assert!(actor.try_send("GO").is_ok(), "first event fills the single slot");
    let second = actor.try_send("GO");
    assert!(matches!(second, Err(Error::LimitExceeded(_))), "got {second:?}");
}

/// `Overflow::DropNewest`: a full mailbox silently drops the new event (no observer hook).
#[tokio::test(start_paused = true)]
async fn overflow_drop_newest_silently_drops() {
    #[derive(Default)]
    struct Drops(Mutex<Vec<String>>);
    impl Observer<DynContext> for Drops {
        fn on_event_dropped(&self, _m: &Machine, event: &Event, reason: &'static str) {
            self.0.lock().unwrap().push(format!("{} {reason}", event.ty));
        }
    }
    let drops = Arc::new(Drops::default());
    let m = machine(
        r#"{"id":"n","initial":"a","states":{"a":{"on":{"GO":"b","LATE":"z"}},"b":{},"z":{}}}"#,
    );
    let actor = spawn_with_observers(
        m,
        MachineLogic::dynamic(),
        Options { mailbox_capacity: 1, overflow: Overflow::DropNewest, ..Options::default() },
        vec![drops.clone()],
    )
    .unwrap();
    assert!(actor.try_send("GO").is_ok(), "fills the slot");
    // The new event is dropped (not queued, not an error).
    assert!(actor.try_send("LATE").is_ok(), "DropNewest reports Ok");
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(actor.view().state_ids, ["n.b"], "the kept event was processed");
    assert!(drops.0.lock().unwrap().is_empty(), "DropNewest does not notify observers");
}

/// `Overflow::Block`: `send` waits for room and both events are processed in order.
#[tokio::test(start_paused = true)]
async fn overflow_block_waits_for_room() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let order2 = Arc::clone(&order);
    let logic = MachineLogic::dynamic().action("mark", move |scope| {
        order2.lock().unwrap().push(scope.event.ty.to_string());
        Ok(())
    });
    let m = machine(r#"{"id":"b","initial":"a","states":{"a":{"on":{"GO":{"actions":"mark"}}}}}"#);
    let actor = spawn(
        m,
        logic,
        Options { mailbox_capacity: 1, overflow: Overflow::Block, ..Options::default() },
    )
    .unwrap();
    assert!(actor.try_send("GO").is_ok(), "fills the slot");
    let waiter = tokio::spawn({
        let a = actor.clone();
        async move { a.send_and_wait("GO").await }
    });
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(waiter.await.unwrap().is_ok(), "Block send_and_wait should eventually succeed");
    assert_eq!(order.lock().unwrap().len(), 2, "both events processed in order");
}

/// `send_and_wait` applies the overflow policy before waiting: under `Reject` a full mailbox is
/// `LimitExceeded` instead of blocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_and_wait_respects_reject_policy() {
    let m = machine(
        r#"{"id":"r","initial":"a","states":{"a":{"on":{"GO":{"target":"a","actions":"slow"}}}}}"#,
    );
    let logic = MachineLogic::dynamic().action("slow", |_| {
        std::thread::sleep(Duration::from_millis(200));
        Ok(())
    });
    let actor = spawn(
        m,
        logic,
        Options { mailbox_capacity: 1, overflow: Overflow::Reject, ..Options::default() },
    )
    .unwrap();
    // Let the actor take the first event (and block in `slow`), then fill the one slot.
    actor.try_send("GO").expect("first send");
    tokio::time::sleep(Duration::from_millis(20)).await;
    actor.try_send("GO").expect("fills the slot while the actor is busy");
    let err = actor.send_and_wait("GO").await.expect_err("Reject must not block");
    assert!(matches!(err, Error::LimitExceeded(_)), "{err:?}");
    actor.stop().await;
}

// ---------------------------------------------------------------------------
// Receipts on finished / stopped actors
// ---------------------------------------------------------------------------

/// A finished (done) actor still answers; events arrive as unhandled, not as errors.
#[tokio::test(start_paused = true)]
async fn send_and_wait_on_done_actor_returns_unhandled_receipt() {
    let actor = spawn(
        machine(r#"{"id":"f","initial":"a","states":{"a":{"on":{"FINISH":"done"}},"done":{"type":"final"}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("FINISH").await.unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    // send (fire-and-forget) to a done actor is not an error.
    assert!(actor.send("NOPE").await.is_ok());
    let receipt = actor.send_and_wait("NOPE").await.unwrap();
    assert!(receipt.unhandled);
    assert_eq!(receipt.status, Status::Done);
    assert_eq!(actor.view().state_ids, ["f.done"]);
}

/// After `stop`: sends, `send_and_wait` and snapshot fail with `InvalidLifecycle`; a second
/// `stop` is a no-op that does not hang.
#[tokio::test(start_paused = true)]
async fn sends_after_stop_and_double_stop() {
    let actor = spawn(
        machine(r#"{"id":"s","initial":"a","states":{"a":{}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    actor.stop().await;
    assert_eq!(actor.view().status, Status::Stopped);
    assert_eq!(actor.wait_done().await.status, Status::Stopped);
    assert!(matches!(actor.send("GO").await, Err(Error::InvalidLifecycle { .. })));
    assert!(matches!(actor.send_and_wait("GO").await, Err(Error::InvalidLifecycle { .. })));
    assert!(matches!(actor.snapshot().await, Err(Error::InvalidLifecycle { .. })));
    // Second stop must be a cheap no-op (no panic, no hang).
    actor.stop().await;
}

/// Restart: a fresh actor on the same machine starts from the initial state.
#[tokio::test(start_paused = true)]
async fn restart_after_stop_is_fresh() {
    let m = machine(r#"{"id":"r","initial":"off","states":{"off":{"on":{"GO":"on"}},"on":{}}}"#);
    let actor = spawn(m.clone(), MachineLogic::dynamic(), Options::default()).unwrap();
    actor.send_and_wait("GO").await.unwrap();
    assert_eq!(actor.view().state_ids, ["r.on"]);
    actor.stop().await;

    let again = spawn(m, MachineLogic::dynamic(), Options::default()).unwrap();
    assert_eq!(again.view().state_ids, ["r.off"]);
    again.send_and_wait("GO").await.unwrap();
    assert_eq!(again.view().state_ids, ["r.on"]);
    again.stop().await;
}

// ---------------------------------------------------------------------------
// Child machine / invoked actor lifecycles
// ---------------------------------------------------------------------------

/// A child whose service fails reaches the parent's `onError` (xstate.error.actor).
#[tokio::test(start_paused = true)]
async fn child_error_reaches_parent_onerror() {
    let logic = MachineLogic::dynamic().async_service("boom", |_ctx, _input| {
        Box::pin(async { Err(Error::logic("child kaboom")) })
    });
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"loading","states":{
                "loading":{"invoke":{"src":"boom","id":"w","onError":{"target":"failed"}}},
                "failed":{"type":"final"}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    assert_eq!(view.state_ids, ["p.failed"]);
}

/// A child whose async service panics reaches the parent as `error.actor` (the panic is
/// contained), so its `onError` transition runs instead of the invoke hanging forever.
#[tokio::test(flavor = "multi_thread")]
async fn child_async_service_panic_reaches_on_error() {
    let logic = MachineLogic::dynamic()
        .async_service("boom", |_ctx, _input| Box::pin(async { panic!("service panicked") }));
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"loading","states":{
                "loading":{"invoke":{"src":"boom","id":"w","onError":"failed","onDone":"ready"}},
                "ready":{},"failed":{"type":"final"}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    assert_eq!(view.state_ids, ["p.failed"], "the panic never reached onError");
    actor.stop().await;
}

/// A panicking action fails only its own actor (the panic is contained into `Status::Error`);
/// a sibling actor keeps working.
#[tokio::test(flavor = "multi_thread")]
async fn panicking_action_fails_only_its_own_actor() {
    let boom_logic = MachineLogic::dynamic().action("boom", |_scope| panic!("action panicked"));
    let boom = spawn(
        machine(r#"{"id":"boom","initial":"a","states":{"a":{"on":{"GO":{"actions":"boom"}}}}}"#),
        boom_logic,
        Options::default(),
    )
    .unwrap();

    let ok = spawn(
        machine(r#"{"id":"ok","initial":"off","states":{"off":{"on":{"TOGGLE":"on"}},"on":{}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();

    // Fire the panicking action; the actor must record an error, not die silently.
    let _ = boom.try_send("GO");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let view = boom.view();
    assert_eq!(view.status, Status::Error, "a panicking action must fail its actor");
    assert!(
        view.error.as_deref().is_some_and(|e| e.contains("panicked")),
        "the panic is not named in the actor's error: {:?}",
        view.error
    );
    // The actor still answers (with an unhandled receipt), rather than being unreachable.
    assert!(boom.send("GO").await.is_ok());

    // The sibling is unaffected.
    let receipt = ok.send_and_wait("TOGGLE").await.unwrap();
    assert_eq!(receipt.state_ids(), ["ok.on"]);
    ok.stop().await;
    boom.stop().await;
}

/// `sendTo` a child stopped by the `stop` action is an unresolved target (soft error).
#[tokio::test(start_paused = true)]
async fn send_to_stopped_child_is_unresolved() {
    let child = machine(r#"{"id":"child","initial":"idle","states":{"idle":{}}}"#);
    let logic = MachineLogic::dynamic().machine_service("child", child);
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"running","states":{
                "running":{"invoke":{"src":"child","id":"w"},
                           "on":{"STOP":{"actions":{"type":"stop","params":{"id":"w"}}},
                                 "PING":{"actions":{"type":"sendTo","params":{"to":"w","event":"HI"}}}}}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("STOP").await.unwrap();
    let receipt = actor.send_and_wait("PING").await.unwrap();
    let err = receipt.error.expect("a dead child sendTo is a soft error");
    assert_eq!(err.kind(), ErrorKind::UnresolvedTarget);
    assert!(err.to_string().starts_with("sendTo"), "{err}");
}

/// `sendParent` from a child whose parent is gone (root has no parent) is unresolved.
#[tokio::test(start_paused = true)]
async fn send_parent_from_root_is_unresolved() {
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"a","states":{"a":{"on":{
                "UP":{"actions":{"type":"sendParent","params":{"event":"HI"}}}}}}}"#,
        ),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    let receipt = actor.send_and_wait("UP").await.unwrap();
    let err = receipt.error.expect("sendParent on a root is a soft error");
    assert_eq!(err.kind(), ErrorKind::UnresolvedTarget);
    assert!(err.to_string().starts_with("sendParent"), "{err}");
}

// ---------------------------------------------------------------------------
// System registry
// ---------------------------------------------------------------------------

/// Two children registered under the same `systemId`: the second is refused and reported
/// to the parent as an actor error (duplicate systemId), not a panic or a silent drop.
#[tokio::test(start_paused = true)]
async fn system_id_collision_is_reported() {
    let child = machine(r#"{"id":"child","initial":"idle","states":{"idle":{}}}"#);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let errors2 = Arc::clone(&errors);
    let logic = MachineLogic::dynamic()
        .machine_service("childA", child.clone())
        .machine_service("childB", child)
        .action("gotError", move |scope| {
            errors2.lock().unwrap().push(scope.event.payload["error"].to_string());
            Ok(())
        });
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"running","states":{
                "running":{"invoke":[
                    {"src":"childA","id":"a","systemId":"shared"},
                    {"src":"childB","id":"b","systemId":"shared",
                     "onError":{"target":"failed","actions":"gotError"}}]},
                "failed":{"type":"final"}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    let view = actor.wait_done().await;
    assert_eq!(view.status, Status::Done);
    assert_eq!(view.state_ids, ["p.failed"]);
    let errs = errors.lock().unwrap();
    assert!(
        errs.iter().any(|e| e.contains("duplicate systemId")),
        "collision error not surfaced: {errs:?}"
    );
}

/// A child's `systemId` is deregistered when the child completes; a later `sendTo` is
/// unresolved.
#[tokio::test(start_paused = true)]
async fn system_id_deregistered_after_child_done() {
    let child = machine(
        r#"{"id":"child","initial":"idle","states":{
            "idle":{"on":{"FINISH":"done"}},"done":{"type":"final"}}}"#,
    );
    let logic = MachineLogic::dynamic().machine_service("child", child);
    let actor = spawn(
        machine(
            r#"{"id":"p","initial":"running","states":{
                "running":{"invoke":{"src":"child","id":"w","systemId":"w","onDone":"finished"},
                           "on":{"KICK":{"actions":{"type":"sendTo","params":{"to":"w","event":"FINISH"}}}}},
                "finished":{"on":{"PING":{"actions":{"type":"sendTo","params":{"to":"w","event":"HI"}}}}}}}"#,
        ),
        logic,
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("KICK").await.unwrap();
    // Let the child process FINISH and the parent process the completion.
    for _ in 0..100 {
        if actor.view().state_ids == ["p.finished"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(actor.view().state_ids, ["p.finished"], "onDone did not fire");
    let receipt = actor.send_and_wait("PING").await.unwrap();
    let err = receipt.error.expect("sendTo a deregistered systemId is a soft error");
    assert_eq!(err.kind(), ErrorKind::UnresolvedTarget);
    assert!(err.to_string().starts_with("sendTo"), "{err}");
}

// ---------------------------------------------------------------------------
// Shutdown drain + observer notifications
// ---------------------------------------------------------------------------

/// Events enqueued before `stop` are processed (drained), not stranded.
#[tokio::test(start_paused = true)]
async fn pending_events_are_processed_before_stop() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let logic = MachineLogic::dynamic().action("mark", move |scope| {
        seen2.lock().unwrap().push(scope.event.ty.to_string());
        Ok(())
    });
    let actor = spawn(
        machine(r#"{"id":"p","initial":"a","states":{"a":{"on":{"GO":{"actions":"mark"}}}}}"#),
        logic,
        Options::default(),
    )
    .unwrap();
    assert!(actor.try_send("GO").is_ok());
    assert!(actor.try_send("GO").is_ok());
    actor.stop().await;
    assert_eq!(seen.lock().unwrap().len(), 2, "queued events were stranded instead of drained");
}

/// Observer lifecycle hooks fire for start, events and stop.
#[tokio::test(start_paused = true)]
async fn observer_lifecycle_notifications() {
    #[derive(Default)]
    struct Rec(Mutex<Vec<String>>);
    impl Observer<DynContext> for Rec {
        fn on_start(
            &self,
            _m: &Machine,
            _s: &xstate_core::ActiveState<DynContext>,
            _r: &xstate_core::StepReport,
        ) {
            self.0.lock().unwrap().push("start".into());
        }
        fn on_event(
            &self,
            _m: &Machine,
            e: &Event,
            _r: &xstate_core::StepReport,
            _s: &xstate_core::ActiveState<DynContext>,
        ) {
            self.0.lock().unwrap().push(format!("event {}", e.ty));
        }
        fn on_stop(&self, _m: &Machine, _s: &xstate_core::ActiveState<DynContext>) {
            self.0.lock().unwrap().push("stop".into());
        }
    }
    let rec = Arc::new(Rec::default());
    let actor = spawn_with_observers(
        machine(r#"{"id":"o","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
        vec![rec.clone()],
    )
    .unwrap();
    actor.send_and_wait("GO").await.unwrap();
    actor.stop().await;
    let log = rec.0.lock().unwrap();
    assert!(log.iter().any(|s| s == "start"), "no on_start: {log:?}");
    assert!(log.iter().any(|s| s == "event GO"), "no on_event: {log:?}");
    assert!(log.iter().any(|s| s == "stop"), "no on_stop: {log:?}");
}

/// A stopped actor's snapshot of a done actor still succeeds (done actors keep answering).
#[tokio::test(start_paused = true)]
async fn done_actor_still_serves_snapshot() {
    let actor = spawn(
        machine(r#"{"id":"s","initial":"a","context":{"n":1},"states":{"a":{"on":{"FINISH":"done"}},"done":{"type":"final"}}}"#),
        MachineLogic::dynamic(),
        Options::default(),
    )
    .unwrap();
    actor.send_and_wait("FINISH").await.unwrap();
    actor.wait_done().await;
    let snap = actor.snapshot().await.unwrap();
    assert_eq!(snap.context["n"], 1);
    actor.stop().await;
}
