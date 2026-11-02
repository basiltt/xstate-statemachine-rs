//! Retry pattern: Python-golden delays and the logic driving a real retry loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use xstate_core::{Error, Machine, SimulatedClock, SyncInterpreter};
use xstate_patterns::{ERRORS_CONTEXT_KEY, JitterMode, RetryPolicy};

const GOLDEN: &str = include_str!("golden/retry_delays.json");

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap_or_else(|e| panic!("{e}"))
}

fn f64_of(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn policy(attempts: u32, base: f64, cap: f64, mode: JitterMode) -> RetryPolicy {
    RetryPolicy::new(attempts, base, 2.0, cap, mode).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn delays_match_the_python_library_for_every_jitter_mode() {
    let golden = golden();
    let params = &golden["params"];
    let sample = f64_of(&golden["rng"]);
    let modes = [
        ("none", JitterMode::None),
        ("full", JitterMode::Full),
        ("equal", JitterMode::Equal),
        ("decorrelated", JitterMode::Decorrelated),
    ];
    for (name, mode) in modes {
        let attempts = u32::try_from(params["max_attempts"].as_u64().unwrap_or(0)).unwrap_or(0);
        let built = RetryPolicy::new(
            attempts,
            f64_of(&params["base_ms"]),
            f64_of(&params["factor"]),
            f64_of(&params["max_ms"]),
            mode,
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .with_rng(move || sample);
        let expected = golden["delays"][name].as_array().unwrap_or_else(|| panic!("no {name}"));
        let mut previous = None;
        for (i, want) in expected.iter().enumerate() {
            let attempt = u32::try_from(i + 1).unwrap_or(1);
            let got = built.delay_after(attempt, previous).unwrap_or_else(|e| panic!("{e}"));
            assert!((got - f64_of(want)).abs() < 1e-9, "{name} attempt {attempt}: {got} vs {want}");
            previous = Some(got);
        }
    }
}

const RETRY_CHART: &str = r#"{
  "id": "retry", "initial": "attempting", "context": { "attempt": 0 },
  "states": {
    "attempting": { "invoke": { "src": "work",
      "onDone":  { "target": "done", "actions": "retryReset" },
      "onError": { "target": "retrying", "actions": "retryBump" } } },
    "retrying": { "after": { "retryDelay": [
      { "guard": "retryCanRetry", "target": "attempting" },
      { "target": "deadLettered" } ] } },
    "done": { "type": "final" },
    "deadLettered": { "type": "final" }
  }
}"#;

fn chart(context: Value) -> Machine {
    let mut config: Value = serde_json::from_str(RETRY_CHART).unwrap_or_else(|e| panic!("{e}"));
    config["context"] = context;
    Machine::from_value(&config).unwrap_or_else(|e| panic!("{e}"))
}

fn run_chart(
    policy: &RetryPolicy,
    failures: usize,
    context: Value,
) -> (SyncInterpreter, SimulatedClock, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let logic = policy.logic().service("work", move |_ctx, _input| {
        if seen.fetch_add(1, Ordering::SeqCst) < failures {
            Err(Error::logic("flaky"))
        } else {
            Ok(json!("ok"))
        }
    });
    let clock = SimulatedClock::new();
    let mut interp = SyncInterpreter::with_clock(chart(context), logic, Arc::new(clock.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    (interp, clock, calls)
}

fn advance(interp: &mut SyncInterpreter, clock: &SimulatedClock, ms: f64) {
    interp.advance(clock, ms).unwrap_or_else(|e| panic!("{e}"));
}

fn attempt(interp: &SyncInterpreter) -> u64 {
    interp.context().and_then(|c| c["attempt"].as_u64()).unwrap_or(u64::MAX)
}

#[test]
fn backoff_delays_gate_each_retry_then_success_resets_the_counter() {
    let policy = policy(4, 100.0, 10_000.0, JitterMode::None);
    let ctx = json!({ "attempt": 0, ERRORS_CONTEXT_KEY: ["earlier"], "attempt_delay_ms": 5 });
    let (mut interp, clock, calls) = run_chart(&policy, 2, ctx);
    assert_eq!(interp.state_ids(), ["retry.retrying"]);
    assert_eq!(attempt(&interp), 1);

    advance(&mut interp, &clock, 99.0);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "retry must wait the full 100 ms");
    advance(&mut interp, &clock, 1.0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(interp.state_ids(), ["retry.retrying"]);
    assert_eq!(attempt(&interp), 2);

    advance(&mut interp, &clock, 199.0);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "second delay is 200 ms");
    advance(&mut interp, &clock, 1.0);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(interp.state_ids(), ["retry.done"]);
    assert_eq!(attempt(&interp), 0);
    let ctx = interp.context().cloned().unwrap_or(Value::Null);
    assert!(ctx.get(ERRORS_CONTEXT_KEY).is_none(), "reset clears the dead-letter chain");
    assert!(ctx.get("attempt_delay_ms").is_none());
}

#[test]
fn exhausting_the_attempts_takes_the_dead_letter_branch() {
    let policy = policy(2, 50.0, 10_000.0, JitterMode::None);
    let (mut interp, clock, calls) = run_chart(&policy, usize::MAX, json!({ "attempt": 0 }));
    advance(&mut interp, &clock, 50.0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    advance(&mut interp, &clock, 100.0);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "no third attempt");
    assert_eq!(interp.state_ids(), ["retry.deadLettered"]);
    assert_eq!(attempt(&interp), 2);
}

#[test]
fn custom_prefix_and_attempt_key_are_honoured() {
    let policy = policy(3, 10.0, 100.0, JitterMode::None);
    let text = RETRY_CHART.replace("retry", "net").replace("\"attempt\"", "\"tries\"");
    let config: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    let machine = Machine::from_value(&config).unwrap_or_else(|e| panic!("{e}"));
    let logic =
        policy.into_logic("net", "tries").service("work", |_ctx, _input| Err(Error::logic("down")));
    let clock = SimulatedClock::new();
    let mut interp = SyncInterpreter::with_clock(machine, logic, Arc::new(clock.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    advance(&mut interp, &clock, 10.0);
    advance(&mut interp, &clock, 20.0);
    assert_eq!(interp.state_ids(), ["net.neting"], "third failure still waits its 40 ms");
    advance(&mut interp, &clock, 40.0);
    assert_eq!(interp.state_ids(), ["net.deadLettered"]);
    assert_eq!(interp.context().map(|c| c["tries"].clone()), Some(json!(3)));
}

#[test]
fn decorrelated_logic_chains_previous_delays_within_bounds() {
    let policy = policy(6, 100.0, 1_000.0, JitterMode::Decorrelated).with_rng(|| 1.0);
    let (mut interp, clock, calls) = run_chart(&policy, usize::MAX, json!({ "attempt": 0 }));
    // rng = 1.0 -> each delay is min(cap, previous * 3): 300, 900, 1000, ...
    for ms in [300.0, 900.0, 1_000.0, 1_000.0, 1_000.0] {
        let before = calls.load(Ordering::SeqCst);
        advance(&mut interp, &clock, ms - 1.0);
        assert_eq!(calls.load(Ordering::SeqCst), before, "fired early at {ms}");
        advance(&mut interp, &clock, 1.0);
        assert_eq!(calls.load(Ordering::SeqCst), before + 1, "did not fire at {ms}");
    }
    // The sixth failure is final: it still waits its (capped) delay, then dead-letters.
    advance(&mut interp, &clock, 999.0);
    assert_eq!(interp.state_ids(), ["retry.retrying"]);
    advance(&mut interp, &clock, 1.0);
    assert_eq!(interp.state_ids(), ["retry.deadLettered"]);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
}

#[test]
fn a_misbehaving_rng_surfaces_as_a_logic_error() {
    let policy = policy(3, 10.0, 100.0, JitterMode::Full).with_rng(|| 7.0);
    let logic = policy.logic().service("work", |_ctx, _input| Err(Error::logic("down")));
    let mut interp = SyncInterpreter::new(chart(json!({ "attempt": 0 })), logic);
    let result = interp.start();
    assert!(result.is_err(), "entering `retrying` evaluates the delay: {result:?}");
}

#[test]
fn decorrelated_bump_publishes_the_previous_delay_in_the_context() {
    let policy = policy(6, 100.0, 1_000.0, JitterMode::Decorrelated).with_rng(|| 1.0);
    let (mut interp, clock, _calls) = run_chart(&policy, usize::MAX, json!({ "attempt": 0 }));
    let key = |interp: &SyncInterpreter| {
        interp.context().and_then(|c| c.get("attempt_delay_ms")).cloned()
    };
    assert_eq!(key(&interp), None, "the first delay has not been bumped into the context yet");
    advance(&mut interp, &clock, 300.0);
    assert_eq!(key(&interp), Some(json!(300.0)), "the second failure publishes delay one");
    advance(&mut interp, &clock, 900.0);
    assert_eq!(key(&interp), Some(json!(900.0)));
}

#[test]
fn a_context_delay_wins_over_the_logic_memory() {
    // A restored (or Python-written) value is the previous delay: 200 * 3 = 600 -> capped 600.
    let policy = policy(6, 100.0, 1_000.0, JitterMode::Decorrelated).with_rng(|| 1.0);
    let ctx = json!({ "attempt": 0, "attempt_delay_ms": 200.0 });
    let (mut interp, clock, calls) = run_chart(&policy, usize::MAX, ctx);
    advance(&mut interp, &clock, 599.0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(&mut interp, &clock, 1.0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
