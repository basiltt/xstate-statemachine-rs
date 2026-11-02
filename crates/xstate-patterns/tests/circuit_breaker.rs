//! Circuit breaker: golden config, the closed / open / half-open lifecycle, and concurrency.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use serde_json::Value;
use xstate_core::{
    ActiveState, DynContext, Event, Machine, MachineLogic, Observer, SimulatedClock, StepReport,
};
use xstate_patterns::{
    AdmitError, CIRCUIT_BREAKER_CONFIG, CallError, CircuitBreaker, CircuitBreakerError,
    CircuitState, circuit_breaker_config, circuit_breaker_logic, wrap,
};

const GOLDEN: &str = include_str!("golden/circuit_breaker_config.json");
const COOLDOWN_MS: f64 = 1_000.0;

fn breaker(threshold: u32, probes: u32) -> (CircuitBreaker, SimulatedClock) {
    let clock = SimulatedClock::new();
    let breaker = CircuitBreaker::builder()
        .failure_threshold(threshold)
        .cooldown_ms(COOLDOWN_MS)
        .half_open_max_calls(probes)
        .clock(Arc::new(clock.clone()))
        .name("payments")
        .build()
        .unwrap_or_else(|e| panic!("{e}"));
    (breaker, clock)
}

fn fail(b: &CircuitBreaker) {
    let r = b.call(|| Err::<(), _>("boom"));
    assert!(matches!(r, Err(CallError::Failed("boom"))), "{r:?}");
}

fn succeed(b: &CircuitBreaker) {
    let r = b.call(|| Ok::<_, &str>(42));
    assert!(matches!(r, Ok(42)), "{r:?}");
}

fn open_it(b: &CircuitBreaker, threshold: u32) {
    for _ in 0..threshold {
        fail(b);
    }
    assert_eq!(b.state(), CircuitState::Open);
}

#[test]
fn config_is_identical_to_the_python_dict() {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(*CIRCUIT_BREAKER_CONFIG, golden);
    assert_eq!(circuit_breaker_config(), golden);
    assert_eq!(
        serde_json::to_string(&golden).unwrap_or_default(),
        serde_json::to_string(&circuit_breaker_config()).unwrap_or_default(),
        "key order matches as well"
    );
}

#[test]
fn config_compiles_with_its_logic() {
    let machine = Machine::from_value(&circuit_breaker_config()).unwrap_or_else(|e| panic!("{e}"));
    let logic: MachineLogic<Value> = circuit_breaker_logic(COOLDOWN_MS);
    logic.validate(&machine).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn opens_after_the_failure_threshold_and_rejects_without_calling() {
    let (b, _clock) = breaker(3, 1);
    fail(&b);
    fail(&b);
    assert_eq!((b.state(), b.failures()), (CircuitState::Closed, 2));
    fail(&b);
    assert_eq!(b.state(), CircuitState::Open);
    assert_eq!(b.opened_count(), 1);

    let calls = AtomicUsize::new(0);
    let r = b.call(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok::<_, &str>(())
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0, "target must not run while open");
    match r {
        Err(CallError::Rejected(AdmitError::Open(e))) => {
            assert_eq!((e.breaker.as_str(), e.state), ("payments", CircuitState::Open));
            assert!(e.to_string().contains("Circuit 'payments' is open"));
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn a_success_resets_the_consecutive_failure_count() {
    let (b, _clock) = breaker(3, 1);
    fail(&b);
    fail(&b);
    succeed(&b);
    assert_eq!(b.failures(), 0);
    fail(&b);
    fail(&b);
    assert_eq!(b.state(), CircuitState::Closed);
}

#[test]
fn half_opens_after_the_cooldown_and_a_successful_probe_closes() {
    let (b, clock) = breaker(2, 1);
    open_it(&b, 2);
    clock.increment(COOLDOWN_MS - 1.0);
    assert_eq!(b.state(), CircuitState::Open, "cooldown not elapsed");
    clock.increment(1.0);
    assert_eq!(b.state(), CircuitState::HalfOpen);
    succeed(&b);
    assert_eq!(b.state(), CircuitState::Closed);
    assert_eq!(b.opened_count(), 1);
}

#[test]
fn a_failed_probe_reopens_and_restarts_the_cooldown() {
    let (b, clock) = breaker(2, 1);
    open_it(&b, 2);
    clock.increment(COOLDOWN_MS);
    assert_eq!(b.state(), CircuitState::HalfOpen);
    fail(&b);
    assert_eq!(b.state(), CircuitState::Open);
    assert_eq!(b.opened_count(), 2);
    clock.increment(COOLDOWN_MS - 1.0);
    assert_eq!(b.state(), CircuitState::Open);
    clock.increment(1.0);
    assert_eq!(b.state(), CircuitState::HalfOpen);
}

#[test]
fn half_open_admits_exactly_the_configured_number_of_probes() {
    let (b, clock) = breaker(1, 2);
    open_it(&b, 1);
    clock.increment(COOLDOWN_MS);
    let first = b.admit().unwrap_or_else(|e| panic!("{e}"));
    let second = b.admit().unwrap_or_else(|e| panic!("{e}"));
    match b.admit() {
        Err(AdmitError::Open(e)) => assert_eq!(e.state, CircuitState::HalfOpen),
        other => panic!("third probe must be refused, got {other:?}"),
    }
    first.record_success();
    assert_eq!(b.state(), CircuitState::Closed);
    // The second probe's result belongs to the earlier window and is ignored.
    second.record_failure();
    assert_eq!(b.state(), CircuitState::Closed);
    assert_eq!(b.failures(), 0);
}

#[test]
fn concurrent_callers_cannot_all_become_the_probe() {
    const THREADS: usize = 16;
    let (b, clock) = breaker(1, 2);
    open_it(&b, 1);
    clock.increment(COOLDOWN_MS);
    let barrier = Barrier::new(THREADS);
    let admitted = AtomicUsize::new(0);
    // Probes stay in flight until every thread has tried: an ignored probe gives its slot back.
    let in_flight = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                barrier.wait();
                if let Ok(permit) = b.admit() {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(permit);
                }
            });
        }
    });
    assert_eq!(admitted.load(Ordering::SeqCst), 2);
}

#[test]
fn a_late_outcome_from_an_earlier_window_is_ignored() {
    let (b, clock) = breaker(2, 1);
    let slow = b.admit().unwrap_or_else(|e| panic!("{e}"));
    open_it(&b, 2);
    clock.increment(COOLDOWN_MS);
    assert_eq!(b.state(), CircuitState::HalfOpen);
    // The slow closed-window call finishes OK: it must NOT close the half-open circuit.
    slow.record_success();
    assert_eq!(b.state(), CircuitState::HalfOpen);
}

#[test]
fn reset_forces_closed_and_keeps_the_lifetime_trip_counter() {
    let (b, _clock) = breaker(2, 1);
    open_it(&b, 2);
    b.reset();
    assert_eq!(b.state(), CircuitState::Closed);
    assert_eq!(b.opened_count(), 1, "rebuilding must not zero the trip counter");
    succeed(&b);
    open_it(&b, 2);
    assert_eq!(b.opened_count(), 2);

    let (c, clock) = breaker(1, 1);
    open_it(&c, 1);
    clock.increment(COOLDOWN_MS);
    assert_eq!(c.state(), CircuitState::HalfOpen);
    c.reset();
    assert_eq!(c.state(), CircuitState::Closed);
}

#[test]
fn reset_makes_in_flight_outcomes_stale() {
    let (b, _clock) = breaker(1, 1);
    let in_flight = b.admit().unwrap_or_else(|e| panic!("{e}"));
    b.reset();
    in_flight.record_failure();
    assert_eq!(b.state(), CircuitState::Closed);
}

#[test]
fn filtered_errors_do_not_touch_the_breaker() {
    let (b, _clock) = breaker(1, 1);
    let r = b.call_filtered(|| Err::<(), _>(404), |status| *status >= 500);
    assert!(matches!(r, Err(CallError::Failed(404))));
    assert_eq!((b.state(), b.failures()), (CircuitState::Closed, 0));
    let r = b.call_filtered(|| Err::<(), _>(503), |status| *status >= 500);
    assert!(matches!(r, Err(CallError::Failed(503))));
    assert_eq!(b.state(), CircuitState::Open);
}

#[test]
fn a_panicking_target_counts_as_a_failure() {
    let (b, _clock) = breaker(1, 1);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = b.call(|| -> Result<(), &str> { panic!("target blew up") });
    }));
    assert!(outcome.is_err());
    assert_eq!(b.state(), CircuitState::Open);
}

#[test]
fn close_stops_the_breaker_and_rejects_calls() {
    let (b, _clock) = breaker(2, 1);
    b.close();
    match b.call(|| Ok::<_, &str>(())) {
        Err(CallError::Rejected(AdmitError::Stopped { breaker })) => {
            assert_eq!(breaker, "payments");
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[test]
fn wrap_routes_every_call_through_the_breaker() {
    let (b, clock) = breaker(2, 1);
    let b = Arc::new(b);
    let lookup = wrap(Arc::clone(&b), |id: u32| if id == 0 { Err("missing") } else { Ok(id * 2) });
    assert_eq!(lookup(21).ok(), Some(42));
    assert!(matches!(lookup(0), Err(CallError::Failed("missing"))));
    assert!(matches!(lookup(0), Err(CallError::Failed("missing"))));
    assert!(matches!(lookup(5), Err(CallError::Rejected(_))));
    clock.increment(COOLDOWN_MS);
    assert_eq!(lookup(5).ok(), Some(10));
    assert_eq!(b.state(), CircuitState::Closed);
}

#[test]
fn builder_rejects_invalid_parameters() {
    let bad = |f: fn(
        xstate_patterns::CircuitBreakerBuilder,
    ) -> xstate_patterns::CircuitBreakerBuilder| {
        let r = f(CircuitBreaker::builder()).build();
        assert!(matches!(r, Err(CircuitBreakerError::Invalid(_))), "{r:?}");
    };
    bad(|b| b.failure_threshold(0));
    bad(|b| b.half_open_max_calls(0));
    bad(|b| b.cooldown_ms(-1.0));
    bad(|b| b.cooldown_ms(f64::NAN));
    bad(|b| b.cooldown_ms(f64::INFINITY));
}

#[test]
fn state_serialises_with_the_chart_names() {
    let json = serde_json::to_string(&CircuitState::HalfOpen).unwrap_or_default();
    assert_eq!(json, "\"half_open\"");
    assert_eq!(CircuitState::HalfOpen.to_string(), "half_open");
}

#[test]
fn interpreter_snapshot_is_reachable_for_diagnostics() {
    let (b, _clock) = breaker(2, 1);
    fail(&b);
    let ids = b.with_interpreter(xstate_core::SyncInterpreter::state_ids);
    assert_eq!(ids, ["payments.closed"]);
}

#[test]
fn an_ignored_probe_gives_its_slot_back() {
    let (b, clock) = breaker(1, 1);
    open_it(&b, 1);
    clock.increment(COOLDOWN_MS);
    let probe = b.admit().unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(b.admit(), Err(AdmitError::Open(_))), "the only slot is taken");
    probe.ignore();
    // Without the release the breaker would stay half-open and reject every call forever.
    let probe = b.admit().unwrap_or_else(|e| panic!("slot not released: {e}"));
    assert!(matches!(b.admit(), Err(AdmitError::Open(_))));
    probe.record_success();
    assert_eq!(b.state(), CircuitState::Closed);
}

#[test]
fn a_filtered_error_in_half_open_does_not_wedge_the_breaker() {
    let (b, clock) = breaker(1, 1);
    open_it(&b, 1);
    clock.increment(COOLDOWN_MS);
    let r = b.call_filtered(|| Err::<(), _>(404), |code| *code >= 500);
    assert!(matches!(r, Err(CallError::Failed(404))));
    assert_eq!(b.state(), CircuitState::HalfOpen, "an uncounted error leaves the state alone");
    succeed(&b);
    assert_eq!(b.state(), CircuitState::Closed);
}

#[test]
fn a_stale_ignored_permit_releases_nothing() {
    let (b, clock) = breaker(1, 1);
    let early = b.admit().unwrap_or_else(|e| panic!("{e}"));
    open_it(&b, 1);
    clock.increment(COOLDOWN_MS);
    let probe = b.admit().unwrap_or_else(|e| panic!("{e}"));
    early.ignore();
    assert!(matches!(b.admit(), Err(AdmitError::Open(_))), "the closed-window permit owns no slot");
    probe.record_success();
}

/// Calls back into the breaker from every callback: a deadlock if run under the lock.
struct Reentrant {
    breaker: Mutex<Option<Arc<CircuitBreaker>>>,
    seen: AtomicUsize,
}

impl Reentrant {
    fn poke(&self) {
        self.seen.fetch_add(1, Ordering::SeqCst);
        let breaker =
            self.breaker.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(breaker) = breaker {
            let _ = breaker.failures();
        }
    }
}

impl Observer<DynContext> for Reentrant {
    fn on_event(&self, _m: &Machine, _e: &Event, _r: &StepReport, _s: &ActiveState<DynContext>) {
        self.poke();
    }
}

#[test]
fn observers_run_after_the_lock_is_released() {
    let observer = Arc::new(Reentrant { breaker: Mutex::new(None), seen: AtomicUsize::new(0) });
    let clock = SimulatedClock::new();
    let breaker = Arc::new(
        CircuitBreaker::builder()
            .failure_threshold(2)
            .cooldown_ms(COOLDOWN_MS)
            .clock(Arc::new(clock.clone()))
            .observer(Arc::clone(&observer) as Arc<dyn Observer<DynContext>>)
            .build()
            .unwrap_or_else(|e| panic!("{e}")),
    );
    *observer.breaker.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(Arc::clone(&breaker));
    open_it(&breaker, 2);
    assert!(observer.seen.load(Ordering::SeqCst) >= 2, "the observer saw the failures");
    // Reaching here at all proves the observer called `failures()` without deadlocking.
}
