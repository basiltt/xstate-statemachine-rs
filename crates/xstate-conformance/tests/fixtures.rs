//! Structural checks over the fixture set: every case parses, names are unique, both oracles
//! are present with one observation per step, and every oracle disagreement is documented.
//! The engine-driving runner is added in Phase 1.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // test helpers may panic

use std::collections::BTreeSet;

use xstate_conformance::{Case, cases_dir, load_cases};

fn all_cases() -> Vec<Case> {
    let cases = load_cases(&cases_dir()).expect("fixtures load");
    assert!(cases.len() >= 20, "expected the seed set, found {}", cases.len());
    cases
}

#[test]
fn case_names_are_unique_and_match_files() {
    let cases = all_cases();
    let names: BTreeSet<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names.len(), cases.len(), "duplicate case names");
}

#[test]
fn every_case_has_both_oracles_with_one_observation_per_step() {
    for case in all_cases() {
        let expected =
            case.expected.as_ref().unwrap_or_else(|| panic!("{}: no expected", case.name));
        for (oracle, exp) in [("xstate", &expected.xstate), ("python", &expected.python)] {
            if exp.unsupported.is_some() {
                assert_eq!(
                    oracle, "python",
                    "{}: only the Python oracle may refuse a chart",
                    case.name
                );
                assert!(
                    case.divergence.is_some(),
                    "{}: unsupported chart needs a divergence note",
                    case.name
                );
                continue;
            }
            assert_eq!(
                exp.steps.len(),
                case.steps.len(),
                "{}: {oracle} oracle observed {} steps for {} scripted",
                case.name,
                exp.steps.len(),
                case.steps.len()
            );
            let initial = exp
                .initial
                .as_ref()
                .unwrap_or_else(|| panic!("{}: {oracle} has no initial", case.name));
            assert!(!initial.state_ids.is_empty(), "{}: {oracle} initial has no states", case.name);
        }
        assert_eq!(expected.xstate.oracle.name, "xstate");
        assert_eq!(expected.python.oracle.name, "xstate-statemachine");
    }
}

#[test]
fn python_expectations_carry_snapshot_vectors() {
    for case in all_cases() {
        let py = &case.expected.as_ref().expect("expected").python;
        if py.unsupported.is_some() {
            continue;
        }
        let hash =
            py.machine_hash.as_deref().unwrap_or_else(|| panic!("{}: no machine_hash", case.name));
        assert_eq!(hash.len(), 16, "{}: machine_hash is not 16 hex chars", case.name);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{}: machine_hash not hex", case.name);
        let snapshot = py.snapshot.as_ref().unwrap_or_else(|| panic!("{}: no snapshot", case.name));
        assert_eq!(snapshot["version"], serde_json::json!(4), "{}: snapshot layout", case.name);
        assert_eq!(
            snapshot["machine_hash"],
            serde_json::json!(hash),
            "{}: hash mismatch",
            case.name
        );
        assert_eq!(
            snapshot["taken_at"],
            serde_json::json!(0.0),
            "{}: taken_at not zeroed",
            case.name
        );
    }
}

#[test]
fn oracle_disagreements_are_documented() {
    for case in all_cases() {
        let expected = case.expected.as_ref().expect("expected");
        let same = expected.python.unsupported.is_none()
            && expected.xstate.initial == expected.python.initial
            && expected.xstate.steps == expected.python.steps;
        match (same, &case.divergence) {
            (false, None) => panic!("{}: oracles disagree without a divergence note", case.name),
            (true, Some(_)) => {
                panic!("{}: documents a divergence that no longer occurs", case.name)
            }
            _ => {}
        }
    }
}
