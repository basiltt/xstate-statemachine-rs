//! Drives every conformance case through the Rust engine and compares with the XState v5
//! oracle (ADR-0001). A failing case prints a side-by-side diff of the first mismatch.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::print_stdout)]

use xstate_conformance::runner::run_case;
use xstate_conformance::{Observation, cases_dir, load_cases};

fn describe(o: &Observation) -> String {
    format!(
        "states={:?} actions={:?} status={} context={} output={:?} error={:?} guards={:?}",
        o.state_ids, o.actions, o.status, o.context, o.output, o.error, o.guards
    )
}

/// JSON `null` and an absent key are indistinguishable after deserialisation, so treat
/// `Some(Null)` as `None` when comparing outputs.
fn norm(v: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    v.filter(|x| !x.is_null())
}

fn same(a: &Observation, b: &Observation) -> bool {
    a.state_ids == b.state_ids
        && a.actions == b.actions
        && a.status == b.status
        && a.context == b.context
        && norm(a.output.as_ref()) == norm(b.output.as_ref())
        && a.error == b.error
        && a.guards == b.guards
}

#[test]
fn rust_engine_matches_xstate_oracle() {
    let cases = load_cases(&cases_dir()).expect("fixtures load");
    let mut failures = Vec::new();
    for case in &cases {
        let expected = &case.expected.as_ref().expect("expected").xstate;
        let outcome = match run_case(case) {
            Ok(o) => o,
            Err(err) => {
                failures.push(format!("{}: engine error: {err}", case.name));
                continue;
            }
        };
        let Some(initial) = expected.initial.as_ref() else {
            failures.push(format!("{}: xstate oracle has no initial observation", case.name));
            continue;
        };
        let mut pairs = vec![("initial".to_owned(), initial, &outcome.initial)];
        for (i, (e, got)) in expected.steps.iter().zip(&outcome.steps).enumerate() {
            pairs.push((format!("step {i}"), e, got));
        }
        for (label, e, got) in pairs {
            if !same(e, got) {
                failures.push(format!(
                    "{} @ {label}\n    xstate: {}\n    rust:   {}",
                    case.name,
                    describe(e),
                    describe(got)
                ));
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases diverge from XState:
{}",
        failures.len(),
        cases.len(),
        failures.join(
            "
"
        )
    );
}
