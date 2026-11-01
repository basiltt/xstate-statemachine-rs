//! Engine benchmarks feeding the perf gate (see `benches/README.md` at the workspace root).
//! Ids match `benches/budgets.json`.

#![allow(missing_docs)] // criterion macros generate undocumented items

use std::sync::Arc;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic, SimulatedClock, SyncInterpreter};

const TOGGLE: &str = r#"{"id":"toggle","initial":"inactive","states":{
  "inactive":{"on":{"TOGGLE":"active"}},"active":{"on":{"TOGGLE":"inactive"}}}}"#;

const GUARDED: &str = r#"{"id":"door","initial":"closed","context":{"opens":0,"locked":false},"states":{
  "closed":{"on":{"OPEN":{"target":"opened","guard":"isUnlocked","actions":[{"type":"assign","params":{"assignment":{"opens":1}}}]}}},
  "opened":{"on":{"CLOSE":"closed"}}}}"#;

const NESTED: &str = r##"{"id":"m","initial":"a","states":{
  "a":{"initial":"a1","states":{"a1":{"initial":"a1x","states":{"a1x":{"on":{"JUMP":"#m.b.b1.b1x"}}}}}},
  "b":{"initial":"b1","states":{"b1":{"initial":"b1x","states":{"b1x":{"on":{"JUMP":"#m.a.a1.a1x"}}}}}}}}"##;

const PARALLEL: &str = r#"{"id":"p","initial":"run","states":{"run":{"type":"parallel","states":{
  "r1":{"initial":"a","states":{"a":{"on":{"T1":"b"}},"b":{"on":{"T1":"a"}}}},
  "r2":{"initial":"c","states":{"c":{"on":{"T2":"d"}},"d":{"on":{"T2":"c"}}}}}}}}"#;

fn chain_machine(n: usize) -> Value {
    let mut states = serde_json::Map::new();
    for i in 0..n {
        let next = if i + 1 < n { format!("s{}", i + 1) } else { "s0".to_owned() };
        states.insert(format!("s{i}"), json!({"on": {"NEXT": next}, "entry": "tick"}));
    }
    json!({"id": "chain", "initial": "s0", "states": states})
}

fn interp(json: &str) -> SyncInterpreter {
    let machine = Machine::from_json(json).expect("machine");
    let logic = MachineLogic::dynamic().guard("isUnlocked", |_, _, _| Ok(true));
    let mut i = SyncInterpreter::with_clock(machine, logic, Arc::new(SimulatedClock::new()));
    i.start().expect("start");
    i
}

fn send_flat(c: &mut Criterion) {
    let mut i = interp(TOGGLE);
    c.bench_function("send_flat", |b| b.iter(|| i.send(black_box("TOGGLE")).expect("send")));
}

fn send_guard_assign(c: &mut Criterion) {
    let mut i = interp(GUARDED);
    let mut open = true;
    c.bench_function("send_guard_assign", |b| {
        b.iter(|| {
            let ev = if open { "OPEN" } else { "CLOSE" };
            open = !open;
            i.send(black_box(ev)).expect("send")
        });
    });
}

fn send_nested_3(c: &mut Criterion) {
    let mut i = interp(NESTED);
    c.bench_function("send_nested_3", |b| b.iter(|| i.send(black_box("JUMP")).expect("send")));
}

fn send_parallel_2(c: &mut Criterion) {
    let mut i = interp(PARALLEL);
    let mut flip = false;
    c.bench_function("send_parallel_2", |b| {
        b.iter(|| {
            flip = !flip;
            i.send(black_box(if flip { "T1" } else { "T2" })).expect("send")
        });
    });
}

fn build_50_states(c: &mut Criterion) {
    let json = serde_json::to_string(&chain_machine(50)).expect("json");
    c.bench_function("build_50_states", |b| {
        b.iter(|| Machine::from_json(black_box(&json)).expect("machine"))
    });
}

fn snapshot_get(c: &mut Criterion) {
    let machine = Machine::from_value(&chain_machine(50)).expect("machine");
    let logic = MachineLogic::dynamic().action("tick", |_| Ok(()));
    let mut i = SyncInterpreter::with_clock(machine, logic, Arc::new(SimulatedClock::new()));
    i.start().expect("start");
    c.bench_function("snapshot_get", |b| b.iter(|| i.snapshot_json().expect("snapshot")));
}

fn snapshot_restore(c: &mut Criterion) {
    let machine = Machine::from_value(&chain_machine(50)).expect("machine");
    let logic = MachineLogic::dynamic().action("tick", |_| Ok(()));
    let mut i = SyncInterpreter::with_clock(
        machine.clone(),
        logic.clone(),
        Arc::new(SimulatedClock::new()),
    );
    i.start().expect("start");
    let text = i.snapshot_json().expect("snapshot");
    c.bench_function("snapshot_restore", |b| {
        b.iter_batched(
            || (machine.clone(), logic.clone()),
            |(m, l)| {
                let snap = xstate_core::Snapshot::from_json(black_box(&text)).expect("parse");
                SyncInterpreter::from_snapshot(
                    m,
                    l,
                    &snap,
                    Arc::new(SimulatedClock::new()),
                    &Default::default(),
                )
                .expect("restore")
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    send_flat,
    send_guard_assign,
    send_nested_3,
    send_parallel_2,
    build_50_states,
    snapshot_get,
    snapshot_restore
);
criterion_main!(benches);
