//! Saga builder: Python-golden JSON equality and runs of the generated charts.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use xstate_core::{Error, Event, MachineLogic, SimulatedClock, Status, SyncInterpreter};
use xstate_patterns::{JitterMode, RetryPolicy, SagaBuilder, SagaError, SagaStep};

fn ok<T>(r: Result<T, SagaError>) -> T {
    r.unwrap_or_else(|e| panic!("{e}"))
}

fn policy(attempts: u32, jitter: JitterMode) -> RetryPolicy {
    RetryPolicy::new(attempts, 100.0, 2.0, 1_000.0, jitter).unwrap_or_else(|e| panic!("{e}"))
}

fn saga_plain() -> SagaBuilder {
    let b = ok(SagaBuilder::new("order_plain"));
    let b = ok(b.step(SagaStep::new("reserve", "reserveStock")));
    let b = ok(b.step(SagaStep::new("charge", "chargeCard")));
    let b = ok(b.step(SagaStep::new("ship", "shipOrder")));
    ok(b.on_failure(["notifyOps"]))
}

fn saga_retry() -> SagaBuilder {
    let retry = policy(3, JitterMode::None);
    let b = ok(SagaBuilder::new("order_retry"));
    let b = ok(b.step(
        SagaStep::new("reserve_stock", "reserveStock")
            .compensate("releaseStock")
            .retry(retry.clone()),
    ));
    let b = ok(b.step(SagaStep::new("charge_card", "chargeCard").compensate("refundCard")));
    ok(b.step(SagaStep::new("ship", "shipOrder").retry(retry)))
}

fn saga_timeouts() -> SagaBuilder {
    let b = ok(ok(SagaBuilder::new("order_timeouts")).start_event("ORDER_PLACED"));
    let b = ok(b.step(
        SagaStep::new("reserve_stock", "reserveStock").compensate("releaseStock").timeout_ms(5000),
    ));
    let b = ok(b.step(SagaStep::new("verify", "verifyFraud").timeout_ms(2500)));
    let b = ok(b.step(
        SagaStep::new("charge_card", "chargeCard")
            .compensate("refundCard")
            .timeout_ms(10_000)
            .retry(policy(2, JitterMode::None)),
    ));
    let b = ok(b.step(SagaStep::new("ship", "shipOrder")));
    ok(b.on_failure(["notifyOps", "pageOncall"]))
}

fn golden(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}"))
}

fn assert_matches_golden(saga: &SagaBuilder, text: &str) {
    let built = ok(saga.build());
    let want = golden(text);
    assert_eq!(built, want);
    // Value equality ignores key order; the emitted text must read the same as Python's too.
    assert_eq!(
        serde_json::to_string(&built).unwrap_or_default(),
        serde_json::to_string(&want).unwrap_or_default(),
        "key order differs from the Python output"
    );
}

#[test]
fn plain_saga_matches_the_python_golden() {
    assert_matches_golden(&saga_plain(), include_str!("golden/saga_plain.json"));
}

#[test]
fn saga_with_retry_policies_matches_the_python_golden() {
    assert_matches_golden(&saga_retry(), include_str!("golden/saga_retry.json"));
}

#[test]
fn saga_with_timeouts_and_compensation_matches_the_python_golden() {
    assert_matches_golden(&saga_timeouts(), include_str!("golden/saga_timeouts.json"));
}

#[test]
fn generated_charts_compile_and_their_logic_is_complete() {
    for (saga, extra) in [
        (saga_plain(), vec!["notifyOps"]),
        (saga_retry(), vec![]),
        (saga_timeouts(), vec!["notifyOps", "pageOncall"]),
    ] {
        let machine = ok(saga.machine());
        let mut logic = saga.logic();
        for service in saga.required_services() {
            logic = logic.service(service, |_ctx, _input| Ok(Value::Null));
        }
        for action in extra {
            logic = logic.action(action, |_scope| Ok(()));
        }
        logic.validate(&machine).unwrap_or_else(|e| panic!("{}: {e}", saga.name()));
    }
}

type Log = Arc<Mutex<Vec<String>>>;

/// Bind every service of `saga` to a closure that logs its name; `failing` names fail.
fn stubbed(saga: &SagaBuilder, failing: &[&'static str], log: &Log) -> MachineLogic<Value> {
    let mut logic = saga.logic();
    for service in saga.required_services() {
        let log = Arc::clone(log);
        let fails = failing.contains(&service);
        let name = service.to_owned();
        logic = logic.service(service, move |_ctx, _input| {
            log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(name.clone());
            if fails { Err(Error::logic("boom")) } else { Ok(json!(format!("{name}-out"))) }
        });
    }
    for action in ["notifyOps", "pageOncall"] {
        let log = Arc::clone(log);
        logic = logic.action(action, move |_scope| {
            log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(action.to_owned());
            Ok(())
        });
    }
    logic
}

fn calls(log: &Log) -> Vec<String> {
    log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

fn context(interp: &SyncInterpreter) -> Value {
    interp.context().cloned().unwrap_or(Value::Null)
}

#[test]
fn happy_path_runs_every_step_in_order_and_completes() {
    let saga = saga_plain();
    let log = Log::default();
    let mut interp = SyncInterpreter::new(ok(saga.machine()), stubbed(&saga, &[], &log));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_plain.completed"]);
    assert_eq!(interp.status(), Status::Done);
    assert_eq!(calls(&log), ["reserveStock", "chargeCard", "shipOrder"]);
    let ctx = context(&interp);
    assert_eq!(
        ctx["results"],
        json!({ "reserve": "reserveStock-out", "charge": "chargeCard-out", "ship": "shipOrder-out" })
    );
    assert_eq!(ctx["compensated"], json!([]));
    assert_eq!(ctx["error"], Value::Null);
}

#[test]
fn a_failing_step_compensates_completed_steps_in_reverse() {
    let saga = saga_retry();
    let log = Log::default();
    // `ship` retries up to 3 attempts; it never succeeds.
    let logic = stubbed(&saga, &["shipOrder"], &log);
    let clock = SimulatedClock::new();
    let mut interp =
        SyncInterpreter::with_clock(ok(saga.machine()), logic, Arc::new(clock.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_retry.steps.shipRetrying"]);
    // Backoff 100 ms, 200 ms, then 400 ms after the third (final) failure, and the rollback runs.
    for ms in [100.0, 200.0, 400.0] {
        interp.advance(&clock, ms).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(interp.state_ids(), ["order_retry.failed"]);
    assert_eq!(
        calls(&log),
        [
            "reserveStock",
            "chargeCard",
            "shipOrder",
            "shipOrder",
            "shipOrder",
            "refundCard",
            "releaseStock"
        ]
    );
    let ctx = context(&interp);
    assert_eq!(ctx["compensated"], json!(["charge_card", "reserve_stock"]));
    assert_eq!(ctx["error"]["step"], "ship");
    assert_eq!(ctx["error"]["reason"], "error");
    assert_eq!(ctx["error"]["message"], "boom");
    assert_eq!(ctx["attempt_ship"], 3);
    assert_eq!(ctx["attempt_reserve_stock"], 0, "success reset the first step counter");
}

#[test]
fn a_failure_before_any_compensable_step_goes_straight_to_failed() {
    let saga = saga_plain();
    let log = Log::default();
    let mut interp =
        SyncInterpreter::new(ok(saga.machine()), stubbed(&saga, &["reserveStock"], &log));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_plain.failed"]);
    assert_eq!(calls(&log), ["reserveStock", "notifyOps"], "on_failure actions run on entry");
}

#[test]
fn a_retried_step_that_recovers_resets_its_counter_and_continues() {
    let saga = saga_retry();
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let log = Log::default();
    let logic = stubbed(&saga, &[], &log).service("reserveStock", move |_ctx, _input| {
        if seen.fetch_add(1, Ordering::SeqCst) < 2 {
            Err(Error::logic("busy"))
        } else {
            Ok(json!("r1"))
        }
    });
    let clock = SimulatedClock::new();
    let mut interp =
        SyncInterpreter::with_clock(ok(saga.machine()), logic, Arc::new(clock.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_retry.steps.reserve_stockRetrying"]);
    interp.advance(&clock, 100.0).unwrap_or_else(|e| panic!("{e}"));
    interp.advance(&clock, 200.0).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_retry.completed"]);
    let ctx = context(&interp);
    assert_eq!(ctx["attempt_reserve_stock"], 0);
    assert_eq!(ctx["results"]["reserve_stock"], "r1");
    assert_eq!(ctx["error"]["reason"], "error", "the last failure stays recorded");
}

#[test]
fn a_failing_compensation_ends_in_compensation_failed() {
    let saga = saga_retry();
    let log = Log::default();
    let logic = stubbed(&saga, &["chargeCard", "releaseStock"], &log);
    let mut interp = SyncInterpreter::new(ok(saga.machine()), logic);
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    // charge_card fails -> compensate reserve_stock -> that fails too.
    assert_eq!(interp.state_ids(), ["order_retry.compensationFailed"]);
    assert_eq!(calls(&log), ["reserveStock", "chargeCard", "releaseStock"]);
    assert_eq!(context(&interp)["error"]["reason"], "compensation");
}

#[test]
fn start_event_gates_the_saga_and_keeps_its_payload() {
    let saga = saga_timeouts();
    let log = Log::default();
    let mut interp = SyncInterpreter::new(ok(saga.machine()), stubbed(&saga, &[], &log));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_timeouts.idle"]);
    assert_eq!(calls(&log).len(), 0);
    assert_eq!(context(&interp)["input"], Value::Null);

    interp
        .send(Event::with_payload("ORDER_PLACED", json!({ "order_id": 7 })))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["order_timeouts.completed"]);
    assert_eq!(context(&interp)["input"], json!({ "order_id": 7 }));
    assert_eq!(calls(&log), ["reserveStock", "verifyFraud", "chargeCard", "shipOrder"]);
}

#[test]
fn required_services_lists_steps_then_compensations_once() {
    assert_eq!(
        saga_timeouts().required_services(),
        ["reserveStock", "verifyFraud", "chargeCard", "shipOrder", "releaseStock", "refundCard"]
    );
}

#[test]
fn declaration_errors_are_loud() {
    let step = |name: &str| SagaStep::new(name, "svc");
    assert!(SagaBuilder::new("not an identifier").is_err());
    assert!(SagaBuilder::new("1abc").is_err());
    assert!(SagaBuilder::new("ok_name\n").is_err());
    for bad in ["", "done.x", "error.x", "xstate.init", "after.5", "a b", "ORDER_*", "x\ty"] {
        assert!(ok(SagaBuilder::new("s")).start_event(bad).is_err(), "{bad:?}");
    }
    let base = ok(ok(SagaBuilder::new("s")).step(step("a")));
    assert!(base.clone().step(step("a")).is_err(), "duplicate step");
    assert!(base.clone().step(step("not ok")).is_err());
    assert!(base.clone().step(SagaStep::new("b", "  ")).is_err(), "blank invoke");
    assert!(base.clone().step(step("b").compensate("")).is_err(), "blank compensate");
    assert!(base.clone().step(step("b").timeout_ms(0)).is_err());
    assert!(base.clone().on_failure([" "]).is_err());
    let retrying = ok(base.clone().step(step("w").retry(policy(2, JitterMode::None))));
    assert!(retrying.step(step("wRetrying")).is_err(), "collides with generated state");
    let with_retrying = ok(base.step(step("wRetrying")));
    assert!(with_retrying.step(step("w").retry(policy(2, JitterMode::None))).is_err());
    assert!(ok(SagaBuilder::new("empty")).build().is_err(), "a saga needs a step");
}

#[test]
fn build_returns_a_fresh_value_each_call() {
    let saga = saga_plain();
    let mut first = ok(saga.build());
    first["id"] = json!("mutated");
    assert_eq!(ok(saga.build())["id"], "order_plain");
}
