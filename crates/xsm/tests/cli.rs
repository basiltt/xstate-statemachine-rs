//! End-to-end tests of the `xsm` binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, run_json, xsm};
use predicates::prelude::*;
use serde_json::Value;

// ------------------------------------------------------------------------------- global flags

#[test]
fn short_and_long_version_flags_print_the_version() {
    for flag in ["-v", "--version"] {
        xsm().arg(flag).assert().success().stdout(predicate::str::starts_with("xsm "));
    }
}

#[test]
fn no_subcommand_is_a_usage_error() {
    xsm().assert().code(2).stderr(predicate::str::contains("Usage"));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    xsm().arg("frobnicate").assert().code(2);
}

#[test]
fn piped_output_has_no_escape_sequences() {
    xsm()
        .arg("validate")
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn global_flags_work_after_the_subcommand() {
    xsm().args(["info", "--plain", "--no-color", "--verbose"]).assert().success();
}

// ------------------------------------------------------------------------------------ validate

#[test]
fn validate_accepts_a_good_machine() {
    xsm()
        .arg("validate")
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"))
        .stdout(predicate::str::contains("Machine:"))
        .stdout(predicate::str::contains("toggle"))
        .stdout(predicate::str::contains("All 1 file(s) are valid."));
}

#[test]
fn validate_reports_names_of_states_events_and_logic() {
    xsm()
        .arg("validate")
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("payment.waiting"))
        .stdout(predicate::str::contains("CONFIRM"))
        .stdout(predicate::str::contains("countItem"))
        .stdout(predicate::str::contains("hasItems"));
}

#[test]
fn validate_fails_on_an_invalid_machine() {
    xsm()
        .arg("validate")
        .arg(fixture("invalid.json"))
        .assert()
        .code(1)
        .stdout(predicate::str::contains("FAIL"))
        .stdout(predicate::str::contains("nowhere"))
        .stderr(predicate::str::contains("1 file(s) had errors."));
}

#[test]
fn validate_prints_a_summary_table_for_several_files() {
    xsm()
        .arg("validate")
        .arg(fixture("toggle.json"))
        .arg(fixture("invalid.json"))
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Result"))
        .stdout(predicate::str::contains("toggle.json"))
        .stdout(predicate::str::contains("invalid.json"));
}

#[test]
fn validate_json_has_the_documented_shape() {
    let value = run_json(
        xsm()
            .arg("validate")
            .arg("--json")
            .arg(fixture("hierarchical.json"))
            .arg(fixture("unreachable.json")),
    );
    let files = value.as_array().unwrap();
    assert_eq!(files.len(), 2);
    let first = &files[0];
    for key in [
        "file",
        "ok",
        "machine",
        "states",
        "events",
        "actions",
        "guards",
        "services",
        "delays",
        "unreachable_states",
        "findings",
    ] {
        assert!(first.get(key).is_some(), "missing key {key}");
    }
    assert_eq!(first["ok"], true);
    assert_eq!(first["machine"], "checkout");
    assert_eq!(first["states"], 6);
    assert_eq!(first["guards"], serde_json::json!(["hasItems"]));
    assert_eq!(first["events"], serde_json::json!(["ADD", "AGAIN", "CONFIRM", "PAY"]));
    assert_eq!(first["unreachable_states"], serde_json::json!([]));
}

#[test]
fn validate_flags_unreachable_states_as_warnings() {
    let value = run_json(xsm().arg("validate").arg("--json").arg(fixture("unreachable.json")));
    let file = &value[0];
    assert_eq!(file["ok"], true, "an unreachable state is a warning, not an error");
    assert_eq!(file["unreachable_states"], serde_json::json!(["island.orphan"]));
    let finding = &file["findings"][0];
    assert_eq!(finding["level"], "warning");
    assert_eq!(finding["path"], "island.orphan");
    assert!(finding["message"].as_str().unwrap().contains("not reachable"));
}

#[test]
fn validate_json_reports_errors_with_exit_one() {
    let mut cmd = xsm();
    cmd.arg("validate").arg("--json").arg(fixture("invalid.json"));
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value[0]["ok"], false);
    assert_eq!(value[0]["findings"][0]["level"], "error");
    assert!(value[0]["findings"][0]["message"].as_str().unwrap().contains("nowhere"));
}

#[test]
fn validate_is_strict_about_unknown_keys_unless_lenient() {
    xsm()
        .arg("validate")
        .arg(fixture("unknown_key.json"))
        .assert()
        .code(1)
        .stdout(predicate::str::contains("entyr"));
    xsm()
        .args(["validate", "--lenient"])
        .arg(fixture("unknown_key.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("entyr"));
}

#[test]
fn validate_reports_a_missing_file_as_a_finding() {
    let value = run_json(xsm().args(["validate", "--json", "no-such-file.json"]));
    assert_eq!(value[0]["ok"], false);
    assert_eq!(value[0]["findings"][0]["message"], "file not found");
}

#[test]
fn validate_reports_invalid_json_with_a_line_number() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, "{\n  \"id\": \n").unwrap();
    let value = run_json(xsm().args(["validate", "--json"]).arg(&path));
    assert_eq!(value[0]["ok"], false);
    assert!(value[0]["findings"][0]["message"].as_str().unwrap().starts_with("invalid JSON"));
    assert!(value[0]["findings"][0]["path"].as_str().unwrap().starts_with("line "));
}

// ------------------------------------------------------------------------------------ inspect

#[test]
fn inspect_prints_the_state_tree_and_tables() {
    xsm()
        .arg("inspect")
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("state tree"))
        .stdout(predicate::str::contains("+ checkout"))
        .stdout(predicate::str::contains("o cart  (initial)  entry: showCart"))
        .stdout(predicate::str::contains("+ payment"))
        .stdout(predicate::str::contains("o waiting  (initial)  after 500"))
        .stdout(predicate::str::contains("@ done"))
        .stdout(predicate::str::contains("transitions (6)"))
        .stdout(predicate::str::contains("hasItems"))
        .stdout(predicate::str::contains("after 500"))
        .stdout(predicate::str::contains("logic to implement"))
        .stdout(predicate::str::contains("countItem"))
        .stdout(predicate::str::contains("actionErrorPolicy"))
        .stdout(predicate::str::contains("maxIterations"))
        .stdout(predicate::str::contains("strictTargets"))
        .stdout(predicate::str::contains("Version  2.1"));
}

#[test]
fn inspect_no_events_skips_the_transitions_table() {
    xsm()
        .args(["inspect", "--no-events"])
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("state tree"))
        .stdout(predicate::str::contains("transitions").not());
}

#[test]
fn inspect_json_includes_hash_policies_and_transitions() {
    let value = run_json(xsm().args(["inspect", "--json"]).arg(fixture("hierarchical.json")));
    assert_eq!(value["machine"], "checkout");
    assert_eq!(value["version"], "2.1");
    assert_eq!(value["structure_hash"].as_str().unwrap().len(), 16);
    assert_eq!(value["policies"]["actionErrorPolicy"], "fail");
    assert_eq!(value["state_kinds"]["checkout.payment"], "compound");
    assert_eq!(value["state_kinds"]["checkout.payment.done"], "final");
    let transitions = value["transitions"].as_array().unwrap();
    assert_eq!(transitions.len(), 6);
    assert_eq!(transitions[1]["event"], "PAY");
    assert_eq!(transitions[1]["guard"], "hasItems");
    assert_eq!(transitions[1]["to"], "checkout.payment");
}

#[test]
fn inspect_fails_for_a_machine_that_does_not_build() {
    xsm()
        .arg("inspect")
        .arg(fixture("invalid.json"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nowhere"));
}

#[test]
fn inspect_live_is_not_available_yet() {
    xsm()
        .args(["inspect", "--live"])
        .arg(fixture("toggle.json"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not implemented"));
}

// ------------------------------------------------------------------------------------ diagram

#[test]
fn diagram_defaults_to_mermaid() {
    xsm()
        .arg("diagram")
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("stateDiagram-v2"))
        .stdout(predicate::str::contains("TOGGLE"));
}

#[test]
fn diagram_supports_plantuml_and_json() {
    xsm()
        .args(["diagram", "-f", "plantuml"])
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("@startuml"));
    let value = run_json(xsm().args(["diagram", "--format", "json"]).arg(fixture("toggle.json")));
    assert_eq!(value["id"], "toggle");
}

#[test]
fn diagram_writes_into_a_directory_named_after_the_machine() {
    let dir = tempfile::tempdir().unwrap();
    for (format, file) in
        [("mermaid", "toggle.mmd"), ("plantuml", "toggle.puml"), ("json", "toggle.json")]
    {
        xsm()
            .args(["diagram", "-f", format, "-o"])
            .arg(dir.path())
            .arg(fixture("toggle.json"))
            .assert()
            .success()
            .stdout(predicate::str::contains("Wrote"));
        assert!(dir.path().join(file).is_file(), "{file} was not written");
    }
    let mermaid = std::fs::read_to_string(dir.path().join("toggle.mmd")).unwrap();
    assert!(mermaid.starts_with("stateDiagram-v2"));
}

#[test]
fn diagram_writes_to_an_explicit_file_creating_parents() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out").join("chart.txt");
    xsm().args(["diagram", "-o"]).arg(&target).arg(fixture("toggle.json")).assert().success();
    assert!(std::fs::read_to_string(target).unwrap().starts_with("stateDiagram-v2"));
}

#[test]
fn diagram_never_lets_a_machine_id_escape_the_output_directory() {
    let dir = tempfile::tempdir().unwrap();
    let machine = dir.path().join("evil.json");
    std::fs::write(&machine, r#"{"id": "../../escape", "initial": "a", "states": {"a": {}}}"#)
        .unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    xsm().args(["diagram", "-o"]).arg(&out).arg(&machine).assert().success();
    let written: Vec<_> = std::fs::read_dir(&out).unwrap().collect();
    assert_eq!(written.len(), 1);
    assert!(!dir.path().join("escape.mmd").exists());
}

// -------------------------------------------------------------------------------------- docs

#[test]
fn docs_prints_a_markdown_page() {
    xsm()
        .arg("docs")
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("# `checkout`"))
        .stdout(predicate::str::contains("```mermaid"))
        .stdout(predicate::str::contains("stateDiagram-v2"))
        .stdout(predicate::str::contains("## States"))
        .stdout(predicate::str::contains("| `payment.waiting` | atomic |"))
        .stdout(predicate::str::contains("## Transitions"))
        .stdout(predicate::str::contains("## Logic to implement"))
        .stdout(predicate::str::contains("**Guards:** `hasItems`"))
        .stdout(predicate::str::contains("## Policies"));
}

#[test]
fn docs_writes_one_page_per_machine_and_survives_a_broken_file() {
    let dir = tempfile::tempdir().unwrap();
    xsm()
        .arg("docs")
        .arg("-o")
        .arg(dir.path())
        .arg(fixture("invalid.json"))
        .arg(fixture("toggle.json"))
        .arg(fixture("hierarchical.json"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("1 file(s) could not be documented"));
    assert!(dir.path().join("toggle.md").is_file());
    assert!(dir.path().join("checkout.md").is_file());
}

// ---------------------------------------------------------------------------------- simulate

#[test]
fn simulate_runs_events_and_prints_history_and_final_state() {
    xsm()
        .args(["simulate", "-e", "TOGGLE"])
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("(start)"))
        .stdout(predicate::str::contains("TOGGLE"))
        .stdout(predicate::str::contains("final state"))
        .stdout(predicate::str::contains("states  active"));
}

#[test]
fn simulate_advances_the_clock_with_plus_n() {
    let value = run_json(
        xsm().args(["sim", "--json", "-e", "ADD,PAY,+500"]).arg(fixture("hierarchical.json")),
    );
    assert_eq!(value["active"], serde_json::json!(["checkout.payment.retry"]));
    assert_eq!(value["clock_ms"], 500.0);
    assert_eq!(value["status"], "running");
    let history = value["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["label"], "ADD");
    assert_eq!(history[0]["actions"], serde_json::json!(["countItem"]));
    assert_eq!(history[1]["actions"], serde_json::json!(["charge"]));
    assert_eq!(history[2]["kind"], "clock");
    assert_eq!(history[2]["after"], serde_json::json!(["checkout.payment.retry"]));
    assert_eq!(value["initial"]["actions"], serde_json::json!(["showCart"]));
}

#[test]
fn simulate_does_not_fire_a_timer_early() {
    let value = run_json(
        xsm().args(["simulate", "--json", "-e", "PAY,+499"]).arg(fixture("hierarchical.json")),
    );
    assert_eq!(value["active"], serde_json::json!(["checkout.payment.waiting"]));
    assert_eq!(value["armed_timers"], serde_json::json!(["payment.waiting:500"]));
}

#[test]
fn simulate_guards_false_blocks_guarded_transitions() {
    let value = run_json(
        xsm()
            .args(["simulate", "--json", "--guards-false", "-e", "PAY"])
            .arg(fixture("hierarchical.json")),
    );
    assert_eq!(value["active"], serde_json::json!(["checkout.cart"]));
    assert_eq!(value["history"][0]["unhandled"], true);
    assert_eq!(value["default_guard"], false);

    let named = run_json(
        xsm()
            .args(["simulate", "--json", "--guards-false=hasItems", "-e", "PAY"])
            .arg(fixture("hierarchical.json")),
    );
    assert_eq!(named["active"], serde_json::json!(["checkout.cart"]));
    assert_eq!(named["guards"]["hasItems"], false);
}

#[test]
fn simulate_hides_the_context_unless_asked() {
    xsm()
        .args(["simulate", "-e", "ADD"])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("secret-123").not())
        .stdout(predicate::str::contains("context").not());
    let hidden = run_json(xsm().args(["simulate", "--json"]).arg(fixture("hierarchical.json")));
    assert!(hidden.get("context").is_none());

    let shown = run_json(
        xsm().args(["simulate", "--json", "--context", "items"]).arg(fixture("hierarchical.json")),
    );
    assert_eq!(shown["context"], serde_json::json!({"items": 0}));
    xsm()
        .args(["simulate", "--context", "token"])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("secret-123"));
}

#[test]
fn simulate_script_changes_guards_mid_run_and_keeps_timers() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        r#"[
            {"guard": "hasItems", "value": false},
            {"send": "PAY"},
            {"guard": "hasItems", "value": true},
            {"send": "PAY"},
            {"guard": "hasItems", "value": false},
            {"clock": 500}
        ]"#,
    )
    .unwrap();
    let value = run_json(
        xsm()
            .args(["simulate", "--json", "--script"])
            .arg(&script)
            .arg(fixture("hierarchical.json")),
    );
    let history = value["history"].as_array().unwrap();
    assert_eq!(history[1]["unhandled"], true, "first PAY is denied");
    assert_eq!(history[3]["after"], serde_json::json!(["checkout.payment.waiting"]));
    // The guard swap restored the interpreter from a snapshot: the armed timer still fires.
    assert_eq!(value["active"], serde_json::json!(["checkout.payment.retry"]));
}

#[test]
fn simulate_script_reset_restarts_on_a_fresh_clock() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(&script, r#"[{"send": "TOGGLE"}, {"clock": 100}, {"reset": true}]"#).unwrap();
    let value = run_json(
        xsm().args(["simulate", "--json", "--script"]).arg(&script).arg(fixture("toggle.json")),
    );
    assert_eq!(value["active"], serde_json::json!(["toggle.inactive"]));
    assert_eq!(value["clock_ms"], 0.0);
    assert_eq!(value["history"][2]["kind"], "reset");
}

#[test]
fn simulate_refuses_bad_input_with_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(&script, r#"[{"undo": true}]"#).unwrap();
    let toggle = fixture("toggle.json");
    xsm().args(["simulate", "-e", "+abc"]).arg(&toggle).assert().code(2);
    xsm().args(["simulate", "-e", "+-5"]).arg(&toggle).assert().code(2);
    xsm().args(["simulate", "--guards-false=nope"]).arg(&toggle).assert().code(2);
    xsm().args(["simulate", "--script"]).arg(&script).arg(&toggle).assert().code(2);
    xsm()
        .args(["simulate", "-e", "TOGGLE", "--script"])
        .arg(&script)
        .arg(&toggle)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be combined"));
}

#[test]
fn simulate_record_writes_an_inspector_recording() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let out = dir.path().join("session.jsonl");
    xsm()
        .args(["simulate", "-e", "TOGGLE,TOGGLE", "--record"])
        .arg(&out)
        .arg(fixture("toggle.json"))
        .assert()
        .success();
    let text = std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("{e}"));
    let kinds: Vec<String> = text
        .lines()
        .map(|line| {
            let msg: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}"));
            msg["type"].as_str().unwrap_or_default().to_owned()
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "@xstate.actor",
            "@xstate.event",
            "@xstate.snapshot",
            "@xstate.event",
            "@xstate.snapshot",
            "@xstate.event",
            "@xstate.snapshot"
        ]
    );
    assert!(text.contains(r#""event":{"type":"TOGGLE"}"#));
    assert!(!text.contains("\"context\":{\""), "context is deny-by-default");
}

#[test]
fn simulate_without_events_prints_the_initial_state_and_a_hint() {
    xsm()
        .arg("simulate")
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("inactive"))
        .stdout(predicate::str::contains("no events given"));
}

#[test]
fn simulate_fails_for_a_machine_that_does_not_build() {
    xsm().arg("simulate").arg(fixture("invalid.json")).assert().code(1);
}

// ------------------------------------------------------------------------------------- paths

#[test]
fn paths_lists_one_path_per_configuration() {
    xsm()
        .arg("paths")
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("4 paths (shortest, guards=true)"))
        .stdout(predicate::str::contains("(initial)"))
        .stdout(predicate::str::contains("payment.waiting"))
        .stdout(predicate::str::contains("PAY,+500"))
        .stdout(predicate::str::contains("PAY,CONFIRM"));
}

#[test]
fn paths_json_describes_the_steps() {
    let value = run_json(xsm().args(["paths", "--json"]).arg(fixture("toggle.json")));
    assert_eq!(value["machine"], "toggle");
    assert_eq!(value["mode"], "shortest");
    let paths = value["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0]["events"], "");
    assert_eq!(paths[1]["final_states"], serde_json::json!(["toggle.active"]));
    let step = &paths[1]["steps"][0];
    assert_eq!(step["event"], "TOGGLE");
    assert_eq!(step["from"], serde_json::json!(["toggle.inactive"]));
    assert_eq!(step["to"], serde_json::json!(["toggle.active"]));
}

#[test]
fn paths_simple_with_both_guard_assumptions_records_them() {
    xsm()
        .args(["paths", "--simple", "--guards", "both"])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("simple, guards=both"))
        .stdout(predicate::str::contains("guards=true"));
}

#[test]
fn paths_respects_max_depth() {
    xsm()
        .args(["paths", "--max-depth", "0"])
        .arg(fixture("toggle.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("1 path "));
}

#[test]
fn paths_weights_by_time() {
    xsm()
        .args(["paths", "--weight", "time"])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("payment.retry"));
}

#[test]
fn paths_rejects_unknown_option_values() {
    xsm().args(["paths", "--guards", "maybe"]).arg(fixture("toggle.json")).assert().code(2);
}

// ------------------------------------------------------------------------------------- info

#[test]
fn info_prints_the_version_and_features() {
    xsm()
        .arg("info")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")))
        .stdout(predicate::str::contains("Snapshot layout"))
        .stdout(predicate::str::contains("validate"));
}

#[test]
fn info_json_has_versions_and_features() {
    let value = run_json(xsm().args(["info", "--json"]));
    assert_eq!(value["name"], "xsm");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["snapshot_layout"], 4);
    assert_eq!(value["crates"]["xstate-core"], env!("CARGO_PKG_VERSION"));
    let commands = value["features"]["commands"].as_array().unwrap();
    for name in ["validate", "inspect", "diagram", "docs", "simulate", "paths", "snapshots", "info"]
    {
        assert!(commands.iter().any(|c| c == name), "missing command {name}");
    }
    assert!(value["features"]["stores"].as_array().unwrap().iter().any(|s| s == "sqlite"));
}
