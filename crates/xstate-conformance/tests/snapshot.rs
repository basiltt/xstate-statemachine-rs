//! Byte-compatibility with the Python library (ADR-0003): the structure hash of every fixture
//! machine must equal Python's `structure_hash`, and — for cases where the two oracles agree
//! on behaviour — the end-of-run snapshot must equal Python's layout-v4 snapshot (volatile
//! wall-clock fields zeroed), both as a parsed value and as text: `Snapshot::to_json()` must
//! equal the text of Python's `get_snapshot()` once the insignificant whitespace of Python's
//! `indent=2` is removed (key order, number formatting and string escaping must all match).
//! A Python-produced snapshot text must also restore through `SyncInterpreter::from_snapshot`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::print_stdout)]

use std::sync::Arc;

use serde_json::Value;
use xstate_conformance::runner::run_case_with_interpreter;
use xstate_conformance::{cases_dir, load_cases};
use xstate_core::{RestoreOptions, SimulatedClock, Snapshot, SyncInterpreter};

fn zero_volatile(snapshot: &mut Value) {
    snapshot["taken_at"] = Value::from(0.0);
    if let Some(deadlines) = snapshot["deadlines"].as_array_mut() {
        for d in deadlines {
            d["due_at_wall"] = Value::from(0.0);
        }
    }
}

/// Remove the insignificant whitespace (everything outside string literals) from JSON text.
fn strip_json_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// The cases with a Python snapshot text whose behaviour the two engines agree on.
fn text_vectors() -> Vec<(xstate_conformance::Case, String)> {
    load_cases(&cases_dir())
        .expect("fixtures load")
        .into_iter()
        .filter_map(|case| {
            let py = &case.expected.as_ref().expect("expected").python;
            if py.unsupported.is_some() || case.divergence.is_some() {
                return None;
            }
            let text = py.snapshot_text.clone()?;
            Some((case, text))
        })
        .collect()
}

#[test]
fn machine_hash_matches_python() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for case in load_cases(&cases_dir()).expect("fixtures load") {
        let py = &case.expected.as_ref().expect("expected").python;
        let Some(expected_hash) = py.machine_hash.as_deref() else { continue };
        let machine = match xstate_core::Machine::from_value(&case.machine) {
            Ok(m) => m,
            Err(e) => {
                failures.push(format!("{}: compile error: {e}", case.name));
                continue;
            }
        };
        checked += 1;
        if machine.structure_hash() != expected_hash {
            failures.push(format!(
                "{}: hash rust={} python={expected_hash}",
                case.name,
                machine.structure_hash()
            ));
        }
    }
    assert!(checked > 30, "too few hash vectors checked ({checked})");
    assert!(failures.is_empty(), "{} hash mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn snapshot_matches_python_where_behaviour_agrees() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for case in load_cases(&cases_dir()).expect("fixtures load") {
        let expected = case.expected.as_ref().expect("expected");
        let py = &expected.python;
        if py.unsupported.is_some() || case.divergence.is_some() {
            continue;
        }
        let Some(expected_snapshot) = py.snapshot.clone() else { continue };
        let (_, interp) = match run_case_with_interpreter(&case) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: engine error: {e}", case.name));
                continue;
            }
        };
        let mut got = serde_json::to_value(interp.snapshot().expect("snapshot")).expect("json");
        zero_volatile(&mut got);
        checked += 1;
        if got != expected_snapshot {
            let mut diffs = Vec::new();
            for (k, v) in expected_snapshot.as_object().unwrap() {
                if got.get(k) != Some(v) {
                    diffs.push(format!(
                        "    {k}: rust={} python={v}",
                        got.get(k).unwrap_or(&Value::Null)
                    ));
                }
            }
            failures.push(format!("{}:\n{}", case.name, diffs.join("\n")));
        }
    }
    assert!(checked > 20, "too few snapshot vectors checked ({checked})");
    assert!(
        failures.is_empty(),
        "{} snapshot mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn snapshot_round_trips_through_json_and_restore() {
    for case in load_cases(&cases_dir()).expect("fixtures load") {
        let Ok((_, interp)) = run_case_with_interpreter(&case) else { continue };
        let snapshot = interp.snapshot().expect("snapshot");
        let text = snapshot.to_json().expect("json");
        let parsed = xstate_core::Snapshot::from_json(&text).expect("parse");
        assert_eq!(parsed, snapshot, "{}: snapshot JSON round trip", case.name);

        let machine = xstate_core::Machine::from_value(&case.machine).expect("machine");
        let logic = xstate_conformance::runner::stub_logic(&case, &machine);
        let restored = xstate_core::SyncInterpreter::from_snapshot(
            machine,
            logic,
            &parsed,
            std::sync::Arc::new(xstate_core::SimulatedClock::new()),
            &xstate_core::RestoreOptions::default(),
        )
        .unwrap_or_else(|e| panic!("{}: restore failed: {e}", case.name));
        assert_eq!(
            restored.state_ids(),
            interp.state_ids(),
            "{}: state ids after restore",
            case.name
        );
        assert_eq!(restored.status(), interp.status(), "{}: status after restore", case.name);
        assert_eq!(
            restored.pending_timers(),
            interp.pending_timers(),
            "{}: timers after restore",
            case.name
        );
    }
}

#[test]
fn snapshot_text_matches_python_text() {
    let mut failures = Vec::new();
    let vectors = text_vectors();
    for (case, python_text) in &vectors {
        let (_, interp) = match run_case_with_interpreter(case) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: engine error: {e}", case.name));
                continue;
            }
        };
        let mut snapshot = interp.snapshot().expect("snapshot");
        snapshot.taken_at = 0.0;
        for d in &mut snapshot.deadlines {
            d.due_at_wall = 0.0;
        }
        let rust_text = snapshot.to_json().expect("json");
        let expected = strip_json_whitespace(python_text);
        if rust_text != expected {
            failures.push(format!(
                "{}:
  rust:   {rust_text}
  python: {expected}",
                case.name
            ));
        }
    }
    assert!(vectors.len() > 20, "too few snapshot text vectors checked ({})", vectors.len());
    assert!(
        failures.is_empty(),
        "{} snapshot text mismatches:
{}",
        failures.len(),
        failures.join(
            "
"
        )
    );
}

#[test]
fn python_snapshot_text_restores() {
    let vectors = text_vectors();
    for (case, python_text) in &vectors {
        let parsed = Snapshot::from_json(python_text)
            .unwrap_or_else(|e| panic!("{}: Python snapshot text does not parse: {e}", case.name));
        let machine = xstate_core::Machine::from_value(&case.machine).expect("machine");
        let logic = xstate_conformance::runner::stub_logic(case, &machine);
        let restored = SyncInterpreter::from_snapshot(
            machine,
            logic,
            &parsed,
            Arc::new(SimulatedClock::new()),
            &RestoreOptions::default(),
        )
        .unwrap_or_else(|e| panic!("{}: restoring the Python snapshot failed: {e}", case.name));
        assert_eq!(
            restored.state_ids(),
            parsed.state_ids,
            "{}: state ids after restoring the Python snapshot",
            case.name
        );
    }
    assert!(vectors.len() > 20, "too few snapshot text vectors restored ({})", vectors.len());
}
