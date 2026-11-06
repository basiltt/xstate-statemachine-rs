//! The direct compiler (`super::direct`) against the parser path: every machine document in
//! the repository (conformance cases, CLI fixtures, agent charts), a set of edge cases and
//! random machines are built both ways; whenever the direct path accepts a document, the two
//! machines must be identical, and `Machine::from_value` must return exactly what the parser
//! path returns (the same machine, or the same error).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Machine, direct, source};
use crate::config::ParseOptions;

/// Every JSON file under `dir` (recursively), skipping build output.
fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() && name != "target" && name != ".git" && name != "node_modules" {
            json_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

/// Machine documents found in the repository: whole files that look like a machine, and the
/// `machine` of every conformance case.
fn repository_documents() -> Vec<(String, Value)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["conformance", "crates", "benches", "docs", "examples"] {
        json_files(&root.join(dir), &mut files);
    }
    let mut docs = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        let name = file.display().to_string();
        if let Some(machine) = value.get("machine") {
            docs.push((format!("{name}#machine"), machine.clone()));
        }
        if value.get("states").is_some() {
            docs.push((name, value));
        }
    }
    docs
}

/// Documents at the edge of what the direct path accepts, valid and invalid.
fn edge_documents() -> Vec<Value> {
    let mut docs = edge_states();
    docs.extend(edge_transitions());
    docs.extend(edge_handlers());
    docs
}

/// State and root edge cases.
fn edge_states() -> Vec<Value> {
    vec![
        json!({"id": "m", "states": {"a": {}}}),
        json!({"id": "m", "states": {}}),
        json!({"id": "m", "type": "parallel", "states": {"r": {"states": {"x": {}}}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"type": "final"}, "b": {}}}),
        json!({"id": "m", "initial": "nope", "states": {"a": {}, "b": {}}}),
        json!({"id": "m", "states": {"a": {}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"type": "compound"}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"type": "final", "states": {"x": {}}}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"type": "Final"}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"type": "history"}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"initial": 3}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"initial": "x"}}}),
        json!({"id": "m", "initial": "a", "states": {"": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": 3}}),
        json!({"id": "", "states": {"a": {}}}),
        json!({"id": "m"}),
        json!({"id": "m", "states": 3}),
        json!({"id": "m", "context": "x", "states": {"a": {}}}),
        json!({"id": "m", "version": 2, "context": {"n": 1}, "states": {"a": {}}}),
        json!({"id": "m", "version": [], "states": {"a": {}}}),
        json!({"id": "m", "strict": true, "states": {"a": {}}}),
        json!({"id": "m", "x-note": 1, "states": {"a": {}}}),
        json!({"id": "m", "states": {"a": {"bogus": 1}}}),
        json!({"id": "m", "states": {"a": {"id": "dup"}, "b": {"id": "dup"}}, "initial": "a"}),
        json!({"id": "m", "initial": "a", "states": {"a": {"id": "", "on": {"E": "b"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"id": "c.d", "on": {"E": "#c.d"}}, "b": {}}}),
    ]
}

/// Transition edge cases on a two-state machine.
fn edge_transitions() -> Vec<Value> {
    let leaf = |on: Value| json!({"id": "m", "initial": "a", "states": {"a": {"on": on}, "b": {}}});
    vec![
        leaf(json!({"E": "b"})),
        leaf(json!({"E": "#m.b"})),
        leaf(json!({"E": ".b"})),
        leaf(json!({"E": "m"})),
        leaf(json!({"E": "#m"})),
        leaf(json!({"E": "missing"})),
        leaf(json!({"E": ""})),
        leaf(json!({"E": null})),
        leaf(json!({"E": {}})),
        leaf(json!({"E": []})),
        leaf(json!({"E": 3})),
        leaf(json!({"E": [3]})),
        leaf(json!({"": "b"})),
        leaf(json!({"*": "b", "foo.*": "b", "foo.bar.*": "a", "done.state.x": "b"})),
        leaf(json!({"E": ["b", {"target": "a", "guard": "g"}, {"target": ["b"]}]})),
        leaf(json!({"E": {"target": "b", "guard": "!g", "actions": ["x", "y"], "reenter": true}})),
        leaf(json!({"E": {"target": "b", "guard": "!", "actions": "x"}})),
        leaf(json!({"E": {"target": "b", "guard": "", "actions": "x"}})),
        leaf(json!({"E": {"target": "b", "guard": "stateIn"}})),
        leaf(json!({"E": {"target": "b", "guard": {"type": "g"}}})),
        leaf(json!({"E": {"target": "b", "cond": "g"}})),
        leaf(json!({"E": {"target": "b", "internal": false}})),
        leaf(json!({"E": {"target": "b", "reenter": "yes"}})),
        leaf(json!({"E": {"target": "b", "description": 3}})),
        leaf(json!({"E": {"target": "b", "description": "go"}})),
        leaf(json!({"E": {"target": "", "actions": "x"}})),
        leaf(json!({"E": {"target": [""]}})),
        leaf(json!({"E": {"target": 3}})),
        leaf(json!({"E": {"actions": "raise"}})),
        leaf(json!({"E": {"actions": ["log"]}})),
        leaf(json!({"E": {"actions": [{"type": "x"}]}})),
        leaf(json!({"E": {"actions": ""}})),
        leaf(json!({"E": {"actions": 0}})),
        leaf(json!({"E": {"actions": null, "target": null}})),
        leaf(json!({"E": {"meta": {}}})),
    ]
}

/// Handler, action and metadata edge cases.
fn edge_handlers() -> Vec<Value> {
    vec![
        json!({"id": "m", "initial": "a", "states": {"a": {"after": {"1e3": "b", "50": "b"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"after": {"1000": "b", "1e3": "b"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"after": {"1.5": "b"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"after": {"slow": "b"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"after": 3}}, "b": {}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"always": "b"}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"always": []}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"always": {"target": "a"}}, "b": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"entry": "x", "exit": ["y", "z"]}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"entry": "assign"}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"description": "d", "on": 3}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"description": 3}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"tags": ["t"]}}}),
        json!({"id": "m", "initial": "a", "on": {"E": ".a"}, "states": {"a": {}}}),
        json!({"id": "m", "initial": "a", "states": {"a": {"initial": "x", "states": {"x": {"on": {"E": "y"}}, "y": {}}}}}),
        json!({"id": "m", "initial": "a.b", "states": {"a.b": {"on": {"E": "#m.a.b"}}}}),
        json!(3),
        json!(null),
    ]
}

/// Everything the compiled machine holds, in a comparable form.
fn fingerprint(m: &Machine) -> String {
    let data = m.data();
    let mut ids: Vec<_> = data.ids.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    ids.sort();
    let mut custom: Vec<_> = data.custom_ids.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    custom.sort();
    let mut index: Vec<_> = data.event_index.iter().map(|(k, v)| format!("{k}={v}")).collect();
    index.sort();
    format!(
        "{}\n{:?}\n{:?}\n{ids:?}\n{custom:?}\n{} {:?} {:?}\n{:?}\n{} {} {} {}\n{:?}\n{:?} {index:?}\n{}\n{}\n{:?}",
        data.id,
        data.states,
        data.transitions,
        data.context,
        data.version,
        data.policies,
        data.warnings,
        data.has_always,
        data.has_history,
        data.has_parallel,
        data.references_logic,
        data.leaf_mask,
        data.event_names,
        m.to_json(),
        m.structure_hash(),
        m.declared_events(),
    )
}

/// The parser path's result for `value`, as `Machine::from_value` would have returned it.
fn parsed(value: &Value) -> crate::Result<Machine> {
    let mut data = Machine::compile_parsed(value, &ParseOptions::default(), None)?;
    data.source = source::Source::from_value(value);
    Ok(Machine { inner: Arc::new(data) })
}

/// Compare both paths on `value`; returns whether the direct path took it.
fn check(name: &str, value: &Value) -> bool {
    let slow = parsed(value);
    let public = Machine::from_value(value);
    match (&slow, &public) {
        (Ok(a), Ok(b)) => assert_eq!(fingerprint(a), fingerprint(b), "{name}: {value}"),
        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{name}: {value}"),
        _ => panic!("{name}: parser {slow:?} vs from_value {public:?} for {value}"),
    }
    let Some(mut data) = direct::compile(value) else { return false };
    data.source = source::Source::from_value(value);
    let direct = Machine { inner: Arc::new(data) };
    let slow = slow.unwrap_or_else(|e| panic!("{name}: direct path accepted, parser refused: {e}"));
    assert_eq!(fingerprint(&slow), fingerprint(&direct), "{name}: {value}");
    assert!(direct.warnings().is_empty(), "{name}");
    true
}

#[test]
fn repository_machines_compile_identically_both_ways() {
    let docs = repository_documents();
    assert!(docs.len() >= 60, "only {} machine documents found", docs.len());
    let direct = docs.iter().filter(|(name, value)| check(name, value)).count();
    // Most conformance machines are plain; the rest exercise the fallback.
    assert!(direct >= 15, "only {direct} of {} documents took the direct path", docs.len());
    assert!(direct < docs.len(), "no document exercised the fallback");
}

#[test]
fn edge_documents_compile_identically_both_ways() {
    let docs = edge_documents();
    let direct = docs.iter().enumerate().filter(|(i, v)| check(&format!("edge {i}"), v)).count();
    assert!(direct >= 20, "only {direct} edge documents took the direct path");
}

#[test]
fn random_machines_compile_identically_both_ways() {
    let mut direct = 0;
    for seed in 0..400 {
        let value = crate::engine::fast_path_tests::random_machine(seed);
        direct += usize::from(check(&format!("seed {seed}"), &value));
    }
    assert!(direct >= 100, "only {direct} random machines took the direct path");
}

#[test]
fn the_benchmark_machines_take_the_direct_path() {
    let machines = [
        json!({"id": "toggle", "initial": "A", "states": {
            "A": {"on": {"NEXT": "B"}}, "B": {"on": {"NEXT": "A"}}}}),
        json!({"id": "counter", "context": {"count": 0}, "initial": "running", "states": {
            "running": {"on": {"INC": {
                "target": "running", "guard": "below_limit", "actions": "increment"}}}}}),
        json!({"id": "hier", "initial": "a", "states": {
            "a": {"initial": "a1", "states": {
                "a1": {"on": {"CROSS": "#hier.b.b2"}}, "a2": {"on": {"DEEP": "#hier.a.a1"}}}},
            "b": {"initial": "b1", "states": {
                "b1": {"on": {"CROSS": "#hier.a.a2"}}, "b2": {"on": {"DEEP": "#hier.b.b1"}}}}}}),
        json!({"id": "delayed", "initial": "waiting", "states": {
            "waiting": {"after": {"1": [{"target": "done"}]}}, "done": {"type": "final"}}}),
    ];
    for (i, m) in machines.iter().enumerate() {
        assert!(check(&format!("bench {i}"), m), "bench machine {i} fell back: {m}");
    }
}
