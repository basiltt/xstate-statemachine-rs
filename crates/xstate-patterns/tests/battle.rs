//! Adversarial battle tests for `xstate-patterns` (0.1.0 pre-release pass).
//!
//! Covers the §6.2 matrix items not already pinned by `retry.rs` / `circuit_breaker.rs` /
//! `saga.rs` / `choreography.rs`:
//!
//! * retry delay bounds per jitter mode over 10 000 samples with a deterministic PRNG, and
//!   `RetryPolicy::apply` merged into a real machine driven by `SimulatedClock`;
//! * the circuit breaker under 100 threads (all-open rejection, exactly `half_open_max_calls`
//!   probes admitted);
//! * a generated 20-step saga (happy path and reverse-order compensation);
//! * a saga step timeout firing through the tokio interpreter (the sync interpreter cannot
//!   wait), and a two-machine choreography ping-pong that trips the loop guard.
//!
//! Everything here is test-only; it never weakens an existing test and never touches `src/`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::{
    DynContext, Error, Machine, MachineLogic, SimulatedClock, Status, SyncInterpreter,
};
use xstate_eda::dispatcher::{Dispatch, EnvelopeHandler, HandlerError};
use xstate_eda::{Envelope, OutboxPlugin, SyncBrokerAdapter, SyncFakeBrokerAdapter};
use xstate_patterns::{
    CallError, CircuitBreaker, CircuitState, JitterMode, RetryPolicy, SagaBuilder, SagaStep,
};
use xstate_persistence::{
    MemoryStore, PersistedOptions, ReceiptFlags, ReceiptJson, StateStore, StoreError, persisted,
};
use xstate_tokio::{Options, spawn};

// ---------------------------------------------------------------------------
// Deterministic PRNG (no external rand needed in this test target)
// ---------------------------------------------------------------------------

/// A tiny xorshift64* / LCG mix returning floats in `[0, 1)`.
fn sampler(seed: u64) -> impl Fn() -> f64 + Send + Sync + 'static {
    let state = Arc::new(Mutex::new(seed));
    move || {
        let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        f64::from_bits(0x3FF0_0000_0000_0000 | ((*s >> 12) & 0x000F_FFFF_FFFF_FFFF)) - 1.0
    }
}

fn policy(base: f64, cap: f64, mode: JitterMode, seed: u64) -> RetryPolicy {
    RetryPolicy::new(20, base, 2.0, cap, mode)
        .unwrap_or_else(|e| panic!("{e}"))
        .with_rng(sampler(seed))
}

#[test]
fn retry_jitter_bounds_hold_over_ten_thousand_samples() {
    // Full jitter: uniform in [0, exp].
    {
        let p = policy(100.0, 1_000.0, JitterMode::Full, 0x1111_1111);
        let mut saw_near_zero = false;
        let mut saw_near_exp = false;
        for attempt in 1..=20 {
            let exp = p.exponential_ms(attempt).unwrap_or_else(|e| panic!("{e}"));
            for _ in 0..500 {
                let d = p.delay_for(attempt).unwrap_or_else(|e| panic!("{e}"));
                assert!((0.0..=exp).contains(&d), "full {d} outside [0, {exp}]");
                assert!(d <= 1_000.0, "full {d} exceeds cap");
                saw_near_zero |= d < exp * 0.05;
                saw_near_exp |= d > exp * 0.95;
            }
        }
        assert!(saw_near_zero && saw_near_exp, "full jitter never sampled both extremes");
    }

    // Equal jitter: uniform in [exp/2, exp].
    {
        let p = policy(100.0, 1_000.0, JitterMode::Equal, 0x2222_2222);
        for attempt in 1..=20 {
            let exp = p.exponential_ms(attempt).unwrap_or_else(|e| panic!("{e}"));
            for _ in 0..500 {
                let d = p.delay_for(attempt).unwrap_or_else(|e| panic!("{e}"));
                assert!((exp / 2.0..=exp).contains(&d), "equal {d} outside [{}, {exp}]", exp / 2.0);
            }
        }
    }

    // Decorrelated jitter: every chained delay stays within [base, cap].
    {
        let p = policy(100.0, 1_000.0, JitterMode::Decorrelated, 0x3333_3333);
        let mut previous = None;
        for _ in 0..10_000 {
            let d = p.delay_after(3, previous).unwrap_or_else(|e| panic!("{e}"));
            assert!((100.0..=1_000.0).contains(&d), "decorrelated {d} outside [100, 1000]");
            previous = Some(d);
        }
    }
}

// ---------------------------------------------------------------------------
// Retry `apply` into a real machine (merges with existing logic)
// ---------------------------------------------------------------------------

#[test]
fn retry_apply_merges_with_existing_logic_and_drives_a_machine() {
    let policy =
        RetryPolicy::new(3, 40.0, 2.0, 1_000.0, JitterMode::None).unwrap_or_else(|e| panic!("{e}"));
    let on_dead = Arc::new(AtomicUsize::new(0));
    let on_dead2 = Arc::clone(&on_dead);
    // `apply` must add the retry names to an ALREADY-populated logic without clobbering it.
    let logic = MachineLogic::dynamic().action("onDead", move |_scope| {
        on_dead2.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let logic = policy
        .apply(logic, "retry", "attempt")
        .service("work", |_ctx, _input| Err(Error::logic("down")));

    let machine = Machine::from_json(
        r#"{"id":"r","initial":"attempting","context":{"attempt":0},"states":{
            "attempting":{"invoke":{"src":"work","onError":{"target":"retrying","actions":"retryBump"}}},
            "retrying":{"after":{"retryDelay":[
                {"guard":"retryCanRetry","target":"attempting"},
                {"target":"deadLettered"}]}},
            "deadLettered":{"type":"final","entry":"onDead"}
        }}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let clock = SimulatedClock::new();
    let mut interp = SyncInterpreter::with_clock(machine, logic, Arc::new(clock.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["r.retrying"], "the first failure lands in retrying");
    // Backoff 40 ms, 80 ms, then 160 ms after the third (final) failure, which dead-letters.
    for ms in [40.0, 80.0, 160.0] {
        interp.advance(&clock, ms).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(interp.state_ids(), ["r.deadLettered"]);
    assert_eq!(on_dead.load(Ordering::SeqCst), 1, "the pre-existing action survived the merge");
}

// ---------------------------------------------------------------------------
// Circuit breaker under 100 threads
// ---------------------------------------------------------------------------

fn breaker(threshold: u32, probes: u32) -> (CircuitBreaker, SimulatedClock) {
    let clock = SimulatedClock::new();
    let breaker = CircuitBreaker::builder()
        .failure_threshold(threshold)
        .cooldown_ms(1_000.0)
        .half_open_max_calls(probes)
        .clock(Arc::new(clock.clone()))
        .name("payments")
        .build()
        .unwrap_or_else(|e| panic!("{e}"));
    (breaker, clock)
}

fn fail(b: &CircuitBreaker) {
    assert!(matches!(b.call(|| Err::<(), _>("boom")), Err(CallError::Failed("boom"))));
}

fn open_it(b: &CircuitBreaker, threshold: u32) {
    for _ in 0..threshold {
        fail(b);
    }
    assert_eq!(b.state(), CircuitState::Open);
}

#[test]
fn breaker_open_rejects_a_hundred_threads_without_running_the_target() {
    const THREADS: usize = 100;
    let (b, _clock) = breaker(1, 1);
    open_it(&b, 1);
    let calls = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);
    let barrier = Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                barrier.wait();
                let r = b.call(|| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), &str>(())
                });
                assert!(
                    matches!(r, Err(CallError::Rejected(_))),
                    "an open breaker must reject, got a non-rejection"
                );
                rejected.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the target must never run while open");
    assert_eq!(rejected.load(Ordering::SeqCst), THREADS);
}

#[test]
fn breaker_half_open_admits_exactly_probe_count_under_a_hundred_threads() {
    const THREADS: usize = 100;
    const PROBES: usize = 3;
    let (b, clock) = breaker(1, u32::try_from(PROBES).unwrap());
    open_it(&b, 1);
    clock.increment(1_000.0);
    assert_eq!(b.state(), CircuitState::HalfOpen);

    let barrier = Barrier::new(THREADS);
    let admitted = AtomicUsize::new(0);
    let permits = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                barrier.wait();
                if let Ok(permit) = b.admit() {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    permits.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(permit);
                }
            });
        }
    });
    assert_eq!(
        admitted.load(Ordering::SeqCst),
        PROBES,
        "exactly half_open_max_calls probes may be admitted, however many threads hammer it"
    );
    // Settle the probes: one success closes the breaker; the rest are stale and ignored.
    let mut permits = permits.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    permits.remove(0).record_success();
    drop(permits);
    assert_eq!(b.state(), CircuitState::Closed);
}

// ---------------------------------------------------------------------------
// Generated 20-step saga
// ---------------------------------------------------------------------------

fn big_saga() -> SagaBuilder {
    let mut saga = SagaBuilder::new("big").unwrap_or_else(|e| panic!("{e}"));
    for i in 0..20 {
        let name = format!("s{i:02}");
        saga = saga
            .step(
                SagaStep::new(name.clone(), format!("do{i:02}")).compensate(format!("undo{i:02}")),
            )
            .unwrap_or_else(|e| panic!("{e}"));
    }
    saga
}

fn big_logic(
    saga: &SagaBuilder,
    failing: &str,
    log: &Arc<Mutex<Vec<String>>>,
) -> MachineLogic<DynContext> {
    let mut logic = saga.logic();
    for service in saga.required_services() {
        let log = Arc::clone(log);
        let fails = service == failing;
        let name = service.to_owned();
        logic = logic.service(service, move |_ctx, _input| {
            log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(name.clone());
            if fails { Err(Error::logic("boom")) } else { Ok(json!(format!("{name}-out"))) }
        });
    }
    logic
}

fn calls_of(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

#[test]
fn twenty_step_saga_runs_every_step_in_order_and_completes() {
    let saga = big_saga();
    let machine = saga.machine().unwrap_or_else(|e| panic!("{e}"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut interp = SyncInterpreter::new(machine, big_logic(&saga, "never", &log));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.status(), Status::Done);
    assert_eq!(interp.state_ids(), ["big.completed"]);
    let expected: Vec<String> = (0..20).map(|i| format!("do{i:02}")).collect();
    assert_eq!(calls_of(&log), expected);
    let ctx = interp.context().cloned().unwrap_or(Value::Null);
    assert_eq!(ctx["results"]["s00"], "do00-out");
    assert_eq!(ctx["results"]["s19"], "do19-out");
}

#[test]
fn twenty_step_saga_compensates_completed_steps_in_reverse() {
    let saga = big_saga();
    let machine = saga.machine().unwrap_or_else(|e| panic!("{e}"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut interp = SyncInterpreter::new(machine, big_logic(&saga, "do10", &log));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(interp.state_ids(), ["big.failed"]);
    // Steps s00..s09 run, s10 fails, then s09..s00 are compensated in reverse.
    let mut expected: Vec<String> = (0..=10).map(|i| format!("do{i:02}")).collect();
    expected.extend((0..10).rev().map(|i| format!("undo{i:02}")));
    assert_eq!(calls_of(&log), expected);
    let ctx = interp.context().cloned().unwrap_or(Value::Null);
    let compensated = ctx["compensated"].as_array().unwrap_or_else(|| panic!("array"));
    assert_eq!(compensated.len(), 10);
    assert_eq!(compensated[0], "s09");
    assert_eq!(compensated[9], "s00");
    assert_eq!(ctx["error"]["step"], "s10");
}

// ---------------------------------------------------------------------------
// Saga runtime timeout through the tokio interpreter
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn saga_step_timeout_fires_through_the_tokio_interpreter() {
    let saga = SagaBuilder::new("timeout_saga")
        .unwrap_or_else(|e| panic!("{e}"))
        .step(SagaStep::new("hang", "hang").timeout_ms(50))
        .unwrap_or_else(|e| panic!("{e}"))
        .step(SagaStep::new("quick", "quick"))
        .unwrap_or_else(|e| panic!("{e}"));
    let machine = saga.machine().unwrap_or_else(|e| panic!("{e}"));
    let logic = saga
        .logic()
        .async_service("hang", |_ctx: DynContext, _input| {
            Box::pin(std::future::pending::<xstate_core::Result<Value>>())
        })
        .service("quick", |_ctx, _input| Ok(json!("ok")));
    let actor = spawn(machine, logic, Options::default()).unwrap_or_else(|e| panic!("{e}"));
    // The `hang` service never completes; the 50 ms `after` must fail the step to `failed`
    // (there is no compensable step, so no compensation runs).
    tokio::time::sleep(Duration::from_millis(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.view().state_ids, ["timeout_saga.failed"], "the timeout did not fire");
    assert_eq!(actor.view().status, Status::Done);
}

// ---------------------------------------------------------------------------
// Choreography: two machines ping-ponging trip the loop guard
// ---------------------------------------------------------------------------

/// A handler that, unlike [`xstate_patterns::PersistedHandler`], delivers the machine's
/// `meta.publish` envelopes back to the bus after each durable step.
struct PublishingHandler {
    store: Arc<dyn StateStore>,
    broker: Arc<SyncFakeBrokerAdapter>,
}

impl EnvelopeHandler for PublishingHandler {
    fn handle(&self, dispatch: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError> {
        let plugin = Arc::new(OutboxPlugin::new().instance_key(dispatch.key.to_owned()));
        let options = PersistedOptions {
            observers: vec![Arc::clone(&plugin) as Arc<dyn xstate_core::Observer<DynContext>>],
            ..PersistedOptions::default()
        };
        let event = dispatch.event.clone();
        let broker = Arc::clone(&self.broker);
        let receipt = persisted(
            self.store.as_ref(),
            dispatch.key,
            dispatch.machine,
            &MachineLogic::dynamic(),
            &options,
            move |interp, session| {
                let receipt = interp.send(event.clone())?;
                let batch = plugin.drain();
                let broker = Arc::clone(&broker);
                session.after_commit(move || {
                    for env in &batch {
                        broker
                            .publish("events", env)
                            .map_err(|e| StoreError::Other(e.to_string()))?;
                    }
                    Ok(())
                });
                Ok(ReceiptJson::from_receipt(&receipt, ReceiptFlags::default(), None))
            },
        )?;
        HandlerError::check_receipt(receipt)
    }
}

fn alpha() -> Machine {
    Machine::from_json(
        r#"{"id":"alpha","initial":"idle","states":{
            "idle":{"on":{"PING":{"target":"idle","meta":{"publish":{"type":"ping.pong"}}}}}
        }}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn beta() -> Machine {
    Machine::from_json(
        r#"{"id":"beta","initial":"idle","states":{
            "idle":{"on":{"PONG":{"target":"idle","meta":{"publish":{"type":"chat.ping"}}}}}
        }}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn two_machines_ping_ponging_trip_the_loop_guard() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
    let broker = Arc::new(SyncFakeBrokerAdapter::new());
    let handler = PublishingHandler { store, broker: Arc::clone(&broker) };
    let routing_table = std::collections::HashMap::from([
        ("chat.ping".to_owned(), xstate_patterns::Route::as_event(alpha(), "PING")),
        ("ping.pong".to_owned(), xstate_patterns::Route::as_event(beta(), "PONG")),
    ]);
    let router = xstate_patterns::ChoreographyRouter::new(routing_table, handler);

    // One seed envelope starts an infinite alpha -> beta -> alpha conversation.
    broker
        .publish(
            "events",
            &Envelope::new("chat.ping")
                .subject("s")
                .data(json!({}))
                .build()
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .unwrap_or_else(|e| panic!("{e}"));

    match router.run_until_quiet(&*broker, Duration::ZERO, 5) {
        Err(xstate_patterns::ChoreographyError::Unsettled(5)) => {}
        other => panic!("expected Unsettled(5), got {other:?}"),
    }
}
