//! The Python library's competitor-benchmark scenarios S1-S7, mirrored one to one (machines,
//! logic and per-operation work as in `benchmarks/competitors/adapters/xstate_sync.py` of
//! xstate-statemachine 0.12.0). Every benchmark reports the time of ONE Python "op":
//!
//! * S1 one `send`; S2 one guarded, assigning `send` (the build is amortised over 20 000 sends,
//!   as Python's `hot(20000)` does); S3 and S4 one loop iteration, i.e. TWO sends;
//! * S5 parse-free build of the `hier` machine from a JSON value + `new` + `start`;
//! * S6 one instance of 1 000: `new` + `start`, then one `send` (instances live in a `Vec`
//!   until the batch is dropped, as in Python's list);
//! * S7 a fresh `SimulatedClock`, build, `new` + `start`, advance 1 ms and observe `done`.
//!
//! Ids match `benches/budgets.json`; `scripts/perf_gate.py` compares them with the Python
//! numbers.

#![allow(missing_docs, clippy::expect_used)] // benchmark harness

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic, SimulatedClock, SyncInterpreter};

fn toggle() -> Value {
    json!({"id": "toggle", "initial": "A", "states": {
        "A": {"on": {"NEXT": "B"}},
        "B": {"on": {"NEXT": "A"}}}})
}

fn counter() -> Value {
    json!({"id": "counter", "context": {"count": 0}, "initial": "running", "states": {
        "running": {"on": {"INC": {
            "target": "running", "guard": "below_limit", "actions": "increment"}}}}})
}

fn hier() -> Value {
    json!({"id": "hier", "initial": "a", "states": {
        "a": {"initial": "a1", "states": {
            "a1": {"on": {"CROSS": "#hier.b.b2"}},
            "a2": {"on": {"DEEP": "#hier.a.a1"}}}},
        "b": {"initial": "b1", "states": {
            "b1": {"on": {"CROSS": "#hier.a.a2"}},
            "b2": {"on": {"DEEP": "#hier.b.b1"}}}}}})
}

fn par() -> Value {
    json!({"id": "par", "type": "parallel", "states": {
        "region1": {"initial": "on1", "states": {
            "on1": {"on": {"TOGGLE1": "off1"}}, "off1": {"on": {"TOGGLE1": "on1"}}}},
        "region2": {"initial": "on2", "states": {
            "on2": {"on": {"TOGGLE2": "off2"}}, "off2": {"on": {"TOGGLE2": "on2"}}}}}})
}

fn delayed() -> Value {
    json!({"id": "delayed", "initial": "waiting", "states": {
        "waiting": {"after": {"1": [{"target": "done"}]}},
        "done": {"type": "final"}}})
}

fn started(config: &Value) -> SyncInterpreter {
    let machine = Machine::from_value(config).expect("machine");
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().expect("start");
    interp
}

fn s1_flat_toggle(c: &mut Criterion) {
    let mut interp = started(&toggle());
    c.bench_function("py_s1_flat_toggle", |b| {
        b.iter(|| interp.send(black_box("NEXT")).expect("send"));
    });
}

/// Python's S2 `hot(n)`: build + start once, then `n` guarded increments.
const S2_SENDS_PER_BUILD: u64 = 20_000;

fn counter_logic(limit: u64) -> MachineLogic<Value> {
    MachineLogic::dynamic()
        .guard("below_limit", move |ctx, _, _| {
            Ok(ctx.get("count").and_then(Value::as_u64).is_some_and(|n| n < limit))
        })
        .action("increment", |scope| {
            if let Some(count) = scope.context.get_mut("count") {
                *count = Value::from(count.as_u64().unwrap_or(0) + 1);
            }
            Ok(())
        })
}

fn s2_guarded_context(c: &mut Criterion) {
    let config = counter();
    c.bench_function("py_s2_guarded_context", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min(S2_SENDS_PER_BUILD);
                let begin = Instant::now();
                let machine = Machine::from_value(black_box(&config)).expect("machine");
                let mut interp = SyncInterpreter::new(machine, counter_logic(n));
                interp.start().expect("start");
                for _ in 0..n {
                    interp.send(black_box("INC")).expect("send");
                }
                total += begin.elapsed();
                assert_eq!(interp.context().and_then(|c| c["count"].as_u64()), Some(n));
                left -= n;
            }
            total
        });
    });
}

fn s3_hierarchical(c: &mut Criterion) {
    let mut interp = started(&hier());
    c.bench_function("py_s3_hierarchical", |b| {
        b.iter(|| {
            interp.send(black_box("CROSS")).expect("send");
            interp.send(black_box("DEEP")).expect("send")
        });
    });
}

fn s4_parallel(c: &mut Criterion) {
    let mut interp = started(&par());
    c.bench_function("py_s4_parallel", |b| {
        b.iter(|| {
            interp.send(black_box("TOGGLE1")).expect("send");
            interp.send(black_box("TOGGLE2")).expect("send")
        });
    });
}

fn s5_construction(c: &mut Criterion) {
    let config = hier();
    c.bench_function("py_s5_construction", |b| {
        b.iter(|| started(black_box(&config)));
    });
}

const S6_INSTANCES: u64 = 1_000;

fn s6_many_instances(c: &mut Criterion) {
    let machine = Machine::from_value(&toggle()).expect("machine");
    c.bench_function("py_s6_many_instances", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min(S6_INSTANCES);
                let begin = Instant::now();
                let mut instances: Vec<SyncInterpreter> = (0..n)
                    .map(|_| {
                        let mut i = SyncInterpreter::new(machine.clone(), MachineLogic::dynamic());
                        i.start().expect("start");
                        i
                    })
                    .collect();
                for interp in &mut instances {
                    interp.send(black_box("NEXT")).expect("send");
                }
                drop(black_box(instances));
                total += begin.elapsed();
                left -= n;
            }
            total
        });
    });
}

fn s7_delayed_transition(c: &mut Criterion) {
    let config = delayed();
    c.bench_function("py_s7_delayed_transition", |b| {
        b.iter(|| {
            let clock = SimulatedClock::new();
            let machine = Machine::from_value(black_box(&config)).expect("machine");
            let mut interp = SyncInterpreter::with_clock(
                machine,
                MachineLogic::dynamic(),
                Arc::new(clock.clone()),
            );
            interp.start().expect("start");
            interp.advance(&clock, 1.0).expect("advance");
            assert!(interp.matches("delayed.done"));
        });
    });
}

criterion_group!(
    benches,
    s1_flat_toggle,
    s2_guarded_context,
    s3_hierarchical,
    s4_parallel,
    s5_construction,
    s6_many_instances,
    s7_delayed_transition
);
criterion_main!(benches);
