//! Graph tests: reachability, shortest / simple paths and coverage targets over hand-written
//! machines and the shared conformance corpus.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xstate_core::graph::{
    GraphOptions, GuardPolicy, Weight, reachable_states, shortest_paths, simple_paths,
    state_coverage_targets, stub_logic, transition_coverage_targets,
};
use xstate_core::{DynContext, Event, Machine, MachineLogic, Status};

fn conformance_machine(case: &str) -> Machine {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/cases")
        .join(format!("{case}.json"));
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    Machine::from_value(&doc["machine"]).unwrap()
}

fn machine(json: &str) -> Machine {
    Machine::from_json(json).unwrap()
}

fn stub(machine: &Machine) -> MachineLogic<DynContext> {
    stub_logic(machine, GuardPolicy::True)
}

fn options(guards: GuardPolicy) -> GraphOptions {
    GraphOptions { guards, ..GraphOptions::default() }
}

#[test]
fn toggle_has_two_configurations_with_shortest_paths_of_length_zero_and_one() {
    let toggle = conformance_machine("001_toggle_basic");
    let logic = stub(&toggle);
    let opts = GraphOptions::default();

    let report = reachable_states(&toggle, &logic, &opts).unwrap();
    assert_eq!(report.configurations.len(), 2);
    assert_eq!(report.unreachable, Vec::<String>::new());
    assert_eq!(report.refused_steps, 0);

    let paths = shortest_paths(&toggle, &logic, &opts).unwrap();
    assert_eq!(paths.iter().map(|p| p.steps.len()).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(paths[1].steps[0].event.ty, "TOGGLE");
    assert_eq!(paths[1].event_string(), "TOGGLE");
    assert_eq!(paths[1].state_ids, paths[1].steps[0].state_ids);
}

#[test]
fn nested_machine_reaches_every_state() {
    let nested = conformance_machine("004_nested_entry_exit_order");
    let report = reachable_states(&nested, &stub(&nested), &GraphOptions::default()).unwrap();
    assert!(report.unreachable.is_empty(), "unreachable: {:?}", report.unreachable);
    let everything: BTreeSet<String> = nested.states().iter().map(|n| n.id.to_string()).collect();
    assert_eq!(report.reachable, everything);
}

#[test]
fn always_transitions_depend_on_the_guard_assumption() {
    let triage = conformance_machine("012_always_transitions");
    let logic = stub(&triage);

    let all_true = reachable_states(&triage, &logic, &options(GuardPolicy::True)).unwrap();
    assert!(all_true.reachable.contains("triage.rejected"));
    assert!(!all_true.reachable.contains("triage.review"));

    let all_false = reachable_states(&triage, &logic, &options(GuardPolicy::False)).unwrap();
    assert!(all_false.reachable.contains("triage.review"));
    assert!(all_false.reachable.contains("triage.approved"));
    assert!(!all_false.reachable.contains("triage.rejected"));

    let both = reachable_states(&triage, &logic, &options(GuardPolicy::Both)).unwrap();
    for state in ["triage.approved", "triage.rejected", "triage.review"] {
        assert!(both.reachable.contains(state), "{state} not reached with Both");
    }
    // `checking` only ever exists inside a macrostep (its `always` transitions settle away),
    // so no stable configuration contains it.
    assert_eq!(both.unreachable, ["triage.checking"]);
}

#[test]
fn guard_assumption_is_recorded_on_each_step() {
    let triage = conformance_machine("012_always_transitions");
    let paths = shortest_paths(&triage, &stub(&triage), &options(GuardPolicy::Both)).unwrap();
    let to_rejected = paths.iter().find(|p| p.state_ids == ["triage.rejected"]).unwrap();
    let to_review = paths.iter().find(|p| p.state_ids == ["triage.review"]).unwrap();
    assert_eq!(to_rejected.steps.last().map(|s| s.guards), Some(true));
    assert_eq!(to_review.steps.last().map(|s| s.guards), Some(false));
}

#[test]
fn orphan_state_is_reported_unreachable_in_document_order() {
    let orphaned = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"on":{"GO":"b"}},
            "lost":{"on":{"BACK":"a"}},
            "b":{},
            "alsoLost":{}}}"#,
    );
    let report = reachable_states(&orphaned, &stub(&orphaned), &GraphOptions::default()).unwrap();
    assert_eq!(report.unreachable, ["m.lost", "m.alsoLost"]);
    assert_eq!(report.reachable, BTreeSet::from(["m".into(), "m.a".into(), "m.b".into()]));
}

#[test]
fn after_timers_are_driven_as_events() {
    let login = conformance_machine("009_after_fixed_delay");
    let stubs = stub(&login);
    let report = reachable_states(&login, &stubs, &GraphOptions::default()).unwrap();
    assert!(report.reachable.contains("login.expired"));

    let paths = shortest_paths(&login, &stubs, &GraphOptions::default()).unwrap();
    let expired = paths.iter().find(|p| p.state_ids == ["login.expired"]).unwrap();
    assert_eq!(expired.steps.len(), 1);
    assert_eq!(expired.steps[0].event.ty, "xstate.after.500.login.active");
    assert_eq!(expired.steps[0].delay_ms, Some(500.0));
    assert_eq!(expired.total_delay_ms().to_bits(), 500.0_f64.to_bits());
    assert_eq!(expired.event_string(), "+500");
    let expired_config =
        report.configurations.iter().find(|c| c.state_ids == ["login.expired"]).unwrap();
    assert_eq!(expired_config.status, Status::Done);
}

#[test]
fn named_delays_use_the_supplied_duration() {
    let m = conformance_machine("011_after_named_delay");
    let logic = stub(&m);
    let unknown = shortest_paths(&m, &logic, &GraphOptions::default()).unwrap();
    let timed = unknown.iter().find(|p| p.total_delay_ms() > 0.0).unwrap();
    assert!((timed.total_delay_ms() - xstate_core::graph::UNKNOWN_DELAY_MS).abs() < f64::EPSILON);

    let delay_name =
        m.states().iter().flat_map(|n| n.after.iter()).map(|a| a.key.clone()).next().unwrap();
    let mut opts = GraphOptions::default();
    opts.named_delays.insert(delay_name, 250.0);
    let known = shortest_paths(&m, &logic, &opts).unwrap();
    let timed = known.iter().find(|p| p.total_delay_ms() > 0.0).unwrap();
    assert_eq!(timed.total_delay_ms().to_bits(), 250.0_f64.to_bits());
}

#[test]
fn time_weight_prefers_the_cheapest_path_over_the_shortest() {
    let racing = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"on":{"FAST":"b"},"after":{"5000":"c"}},
            "b":{"on":{"NEXT":"c"}},
            "c":{}}}"#,
    );
    let logic = stub(&racing);
    let to_c = |weight| {
        let opts = GraphOptions { weight, ..GraphOptions::default() };
        let paths = shortest_paths(&racing, &logic, &opts).unwrap();
        paths.into_iter().find(|p| p.state_ids == ["m.c"]).unwrap()
    };
    let by_steps = to_c(Weight::Steps);
    assert_eq!(by_steps.steps.len(), 1);
    assert_eq!(by_steps.total_delay_ms().to_bits(), 5000.0_f64.to_bits());
    let by_time = to_c(Weight::Time);
    assert_eq!(by_time.event_string(), "FAST,NEXT");
    assert_eq!(by_time.total_delay_ms().to_bits(), 0.0_f64.to_bits());
}

#[test]
fn simple_paths_never_revisit_a_configuration() {
    let diamond = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"on":{"X":"b","Y":"c"}},
            "b":{"on":{"X":"c","BACK":"a"}},
            "c":{"on":{"BACK":"a"}}}}"#,
    );
    let paths = simple_paths(&diamond, &stub(&diamond), &GraphOptions::default()).unwrap();
    let rendered: BTreeSet<String> =
        paths.iter().map(xstate_core::graph::Path::event_string).collect();
    assert_eq!(rendered, BTreeSet::from(["X".into(), "Y".into(), "X,X".into()]));
    assert!(paths.iter().all(|p| !p.steps.is_empty()));

    let limited = simple_paths(
        &diamond,
        &stub(&diamond),
        &GraphOptions { max_paths: 2, ..GraphOptions::default() },
    )
    .unwrap();
    assert_eq!(limited.len(), 2);

    let shallow = simple_paths(
        &diamond,
        &stub(&diamond),
        &GraphOptions { max_depth: 1, ..GraphOptions::default() },
    )
    .unwrap();
    assert!(shallow.iter().all(|p| p.steps.len() == 1));
}

#[test]
fn max_depth_bounds_reachability() {
    let chain = machine(
        r#"{"id":"m","initial":"a","states":{
            "a":{"on":{"N":"b"}},"b":{"on":{"N":"c"}},"c":{}}}"#,
    );
    let shallow = GraphOptions { max_depth: 1, ..GraphOptions::default() };
    let report = reachable_states(&chain, &stub(&chain), &shallow).unwrap();
    assert_eq!(report.unreachable, ["m.c"]);
}

#[test]
fn context_is_part_of_the_identity_unless_ignored() {
    let counter = machine(
        r#"{"id":"m","initial":"a","context":{"n":0},"states":{
            "a":{"on":{"ONE":{"target":"a","actions":[{"type":"assign","params":{"assignment":{"n":1}}}]}}}}}"#,
    );
    let logic = stub(&counter);
    let keyed = reachable_states(&counter, &logic, &GraphOptions::default()).unwrap();
    assert_eq!(keyed.configurations.len(), 2);
    assert_eq!(keyed.configurations[1].context, json!({"n": 1}));

    let ignoring = GraphOptions { key_by_context: false, ..GraphOptions::default() };
    let unkeyed = reachable_states(&counter, &logic, &ignoring).unwrap();
    assert_eq!(unkeyed.configurations.len(), 1);
}

#[test]
fn explicit_events_replace_the_derived_set() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b","STAY":"c"}},"b":{},"c":{}}}"#,
    );
    let opts = GraphOptions { events: Some(vec![Event::new("GO")]), ..GraphOptions::default() };
    let report = reachable_states(&m, &stub(&m), &opts).unwrap();
    assert_eq!(report.unreachable, ["m.c"]);
}

#[test]
fn wildcard_handlers_are_exercised() {
    let m = conformance_machine("013_wildcard_and_partial");
    let report = reachable_states(&m, &stub(&m), &GraphOptions::default()).unwrap();
    assert!(report.unreachable.is_empty(), "unreachable: {:?}", report.unreachable);
}

#[test]
fn invoked_services_complete_or_fail_by_policy() {
    let ok_only = conformance_machine("019_invoke_onerror");
    let logic = stub(&ok_only);
    let done = reachable_states(&ok_only, &logic, &options(GuardPolicy::True)).unwrap();
    let both = reachable_states(&ok_only, &logic, &options(GuardPolicy::Both)).unwrap();
    assert!(both.reachable.len() > done.reachable.len());
    assert!(both.unreachable.len() < done.unreachable.len());
}

#[test]
fn history_pseudo_states_are_never_reported_unreachable() {
    let m = conformance_machine("006_history_shallow");
    let report = reachable_states(&m, &stub(&m), &GraphOptions::default()).unwrap();
    for id in &report.unreachable {
        assert!(!m.node(m.state_by_id(id).unwrap()).is_history());
    }
    assert!(report.unreachable.is_empty(), "unreachable: {:?}", report.unreachable);
}

#[test]
fn exceeding_max_configs_is_an_error() {
    let m = machine(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"N":"b"}},"b":{"on":{"N":"c"}},"c":{}}}"#,
    );
    let opts = GraphOptions { max_configs: 2, ..GraphOptions::default() };
    let err = reachable_states(&m, &stub(&m), &opts).unwrap_err();
    assert_eq!(err.kind(), xstate_core::ErrorKind::LimitExceeded);
}

#[test]
fn missing_implementations_propagate() {
    let triage = conformance_machine("012_always_transitions");
    let err = reachable_states(&triage, &MachineLogic::dynamic(), &GraphOptions::default());
    // Guards are overridden by the policy, so the bare logic suffices for this machine.
    assert!(err.is_ok());
    let delayed = conformance_machine("011_after_named_delay");
    let err =
        reachable_states(&delayed, &MachineLogic::dynamic(), &GraphOptions::default()).unwrap_err();
    assert_eq!(err.kind(), xstate_core::ErrorKind::ImplementationMissing);
}

#[test]
fn coverage_targets_list_every_transition_and_state() {
    let toggle = conformance_machine("001_toggle_basic");
    let targets = transition_coverage_targets(&toggle);
    assert_eq!(targets.len(), 2);
    let edges: BTreeSet<(String, String, Vec<String>)> =
        targets.into_iter().map(|t| (t.source, t.event, t.targets)).collect();
    assert!(edges.contains(&(
        "toggle.inactive".to_owned(),
        "TOGGLE".to_owned(),
        vec!["toggle.active".to_owned()]
    )));
    assert_eq!(state_coverage_targets(&toggle), ["toggle.inactive", "toggle.active"]);

    let triage = conformance_machine("012_always_transitions");
    let always =
        transition_coverage_targets(&triage).into_iter().filter(|t| t.event == "always").count();
    assert_eq!(always, 3);
}

#[test]
fn stub_logic_satisfies_every_conformance_machine() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases");
    let mut checked = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let m = Machine::from_value(&doc["machine"]).unwrap();
        let logic = stub(&m);
        logic.validate(&m).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let report = reachable_states(&m, &logic, &options(GuardPolicy::Both))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_ne!(report.configurations.len(), 0);
        let paths = shortest_paths(&m, &logic, &options(GuardPolicy::Both)).unwrap();
        assert_eq!(paths.len(), report.configurations.len(), "{}", path.display());
        checked += 1;
    }
    assert!(checked >= 30);
}
