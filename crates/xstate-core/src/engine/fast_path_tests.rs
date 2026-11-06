//! Differential test of the plain-microstep fast path: random small machines driven by random
//! event sequences, once with the fast path and once with every microstep on the general
//! path, must produce identical receipts, runtime state and action traces.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Map, Value, json};

use super::algorithm::FAST_PATH;
use crate::{Machine, MachineLogic, SyncInterpreter};

const EVENTS: [&str; 4] = ["E0", "E1", "E2", "E3"];

/// xorshift64*: deterministic choices from a proptest seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// The `states` of a generated subtree under `path`; every state's full id goes to `ids`.
fn build_states(
    rng: &mut Rng,
    path: &str,
    depth: u32,
    ids: &mut Vec<String>,
) -> Map<String, Value> {
    let count = 2 + rng.below(2);
    let mut states = Map::new();
    for i in 0..count {
        let key = format!("s{i}");
        let id = format!("{path}.{key}");
        ids.push(id.clone());
        let mut node = Map::new();
        if depth < 3 && rng.chance(40) {
            if rng.chance(30) {
                node.insert("type".into(), json!("parallel"));
            } else {
                node.insert("initial".into(), json!("s0"));
            }
            let mut kids = build_states(rng, &id, depth + 1, ids);
            if rng.chance(10) {
                kids.insert("hist".into(), json!({"type": "history", "history": "deep"}));
            }
            node.insert("states".into(), Value::Object(kids));
        } else if i > 0 && rng.chance(10) {
            node.insert("type".into(), json!("final"));
        }
        for key in ["entry", "exit"] {
            if rng.chance(12) {
                node.insert(key.into(), json!([format!("{key}_{}", id.replace('.', "_"))]));
            }
        }
        states.insert(key, Value::Object(node));
    }
    states
}

/// A random transition object targeting one of `ids` (or nothing).
fn transition(rng: &mut Rng, ids: &[String]) -> Value {
    let mut t = Map::new();
    if !rng.chance(20) {
        let target = &ids[rng.below(ids.len())];
        t.insert("target".into(), json!(format!("#{target}")));
    }
    if rng.chance(30) {
        t.insert("guard".into(), json!(if rng.chance(50) { "even" } else { "small" }));
    }
    if rng.chance(50) {
        t.insert("actions".into(), json!(["inc"]));
    }
    if rng.chance(20) {
        t.insert("reenter".into(), json!(true));
    }
    Value::Object(t)
}

/// Add `on` handlers (and a few guarded `always`) to every state of `states`.
fn add_transitions(rng: &mut Rng, states: &mut Map<String, Value>, ids: &[String]) {
    for node in states.values_mut() {
        let Some(node) = node.as_object_mut() else { continue };
        if node.get("type") == Some(&json!("history")) {
            continue;
        }
        let mut on = Map::new();
        for ev in EVENTS {
            if rng.chance(35) {
                on.insert(ev.into(), transition(rng, ids));
            }
        }
        if !on.is_empty() {
            node.insert("on".into(), Value::Object(on));
        }
        if rng.chance(5) {
            let mut always = transition(rng, ids);
            always["guard"] = json!("small");
            always["actions"] = json!(["inc"]);
            node.insert("always".into(), always);
        }
        if let Some(Value::Object(children)) = node.get_mut("states") {
            add_transitions(rng, children, ids);
        }
    }
}

/// A random machine of up to three levels (shared with `machine::direct_tests`).
pub(crate) fn random_machine(seed: u64) -> Value {
    let mut rng = Rng(seed | 1);
    let mut ids = Vec::new();
    let mut states = build_states(&mut rng, "m", 1, &mut ids);
    add_transitions(&mut rng, &mut states, &ids);
    let mut root = json!({"id": "m", "context": {"n": 0}, "states": states});
    if rng.chance(25) {
        root["type"] = json!("parallel");
    } else {
        root["initial"] = json!("s0");
    }
    root
}

fn count(ctx: &Value) -> u64 {
    ctx.get("n").and_then(Value::as_u64).unwrap_or(0)
}

fn logic() -> MachineLogic<Value> {
    let mut logic = MachineLogic::dynamic()
        .guard("even", |ctx, _, _| Ok(count(ctx).is_multiple_of(2)))
        .guard("small", |ctx, _, _| Ok(count(ctx) < 3))
        .action("inc", |scope| {
            let n = count(scope.context);
            scope.context["n"] = json!(n + 1);
            Ok(())
        });
    // Entry / exit actions are named after their state: register every name the generator
    // can produce.
    for name in entry_exit_names() {
        logic = logic.action(name, |_| Ok(()));
    }
    logic
}

fn entry_exit_names() -> Vec<String> {
    let mut paths = vec!["m".to_owned()];
    let mut all = Vec::new();
    for _ in 0..3 {
        let next: Vec<String> =
            paths.iter().flat_map(|p| (0..3).map(move |i| format!("{p}_s{i}"))).collect();
        all.extend(next.iter().cloned());
        paths = next;
    }
    all.iter().flat_map(|p| [format!("entry_{p}"), format!("exit_{p}")]).collect()
}

/// Everything observable after one operation: the receipt and the runtime state.
fn observe(interp: &SyncInterpreter<Value>, receipt: &crate::Result<crate::Receipt>) -> String {
    let receipt = match receipt {
        Ok(r) => format!(
            "{:?} changed={} unhandled={} actions={:?} status={:?} err={:?}",
            r.state_ids(),
            r.changed,
            r.unhandled,
            r.action_names(),
            r.status,
            r.error.as_ref().map(ToString::to_string)
        ),
        Err(e) => format!("error {e}"),
    };
    let Some(s) = interp.state() else { return receipt };
    let history: BTreeMap<_, _> = s.history.iter().collect();
    format!(
        "{receipt} | {:?} ctx={} hist={history:?} {:?} out={} err={:?} q={} mut={} seq={} at={:?}",
        s.configuration,
        s.context,
        s.status,
        s.output,
        s.error,
        s.internal_queue.len(),
        s.mutations,
        s.entry_seq,
        s.entered_at
    )
}

/// The trace of one run, or `None` when the generated machine does not compile.
fn run(config: &Value, events: &[u8], fast: bool) -> Option<Vec<String>> {
    FAST_PATH.with(|f| f.set(fast));
    let trace = Machine::from_value(config).ok().map(|machine| {
        let mut interp = SyncInterpreter::new(machine, logic());
        let started = interp.start();
        let mut trace = vec![observe(&interp, &started)];
        for &e in events {
            let receipt = interp.send(EVENTS[usize::from(e) % EVENTS.len()]);
            trace.push(observe(&interp, &receipt));
        }
        trace
    });
    FAST_PATH.with(|f| f.set(true));
    trace
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    #[test]
    fn fast_path_matches_general_path(
        seed in any::<u64>(),
        events in prop::collection::vec(0u8..4, 0..24),
    ) {
        let config = random_machine(seed);
        let general = run(&config, &events, false);
        let fast = run(&config, &events, true);
        prop_assert_eq!(general, fast, "machine: {}", config);
    }
}

#[test]
fn generated_machines_exercise_the_fast_path() {
    let (mut plain, mut built) = (0, 0);
    for seed in 0..200 {
        let Ok(machine) = Machine::from_value(&random_machine(seed)) else { continue };
        built += 1;
        if let Some(plans) = machine.plans() {
            plain += (0..machine.transitions().len())
                .filter(|&i| plans.is_plain(crate::TransitionId::from_index(i)))
                .count();
        }
    }
    assert!(built > 150, "only {built} of 200 generated machines compiled");
    assert!(plain > 300, "only {plain} plain transitions generated");
}
