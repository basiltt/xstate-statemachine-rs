//! Adoption: building a legal snapshot from a bare set of state ids.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_core::{
    Error, Machine, MachineLogic, RealClock, RestartTimers, RestoreOptions, Snapshot, Status,
    SyncInterpreter,
};
use xstate_persistence::StoreError;
use xstate_persistence::adopt::from_state_ids;

const NOW: f64 = 1_700_000_000.0;

fn machine(doc: &Value) -> Machine {
    Machine::from_value(doc).unwrap()
}

fn order() -> Machine {
    machine(&json!({"id": "order", "version": "2.0", "initial": "cart",
        "context": {"currency": "EUR", "items": 0},
        "states": {
            "cart": {"on": {"PAY": "review"}},
            "review": {"initial": "legal", "states": {
                "legal": {"initial": "pending", "states": {"pending": {}, "ok": {}}},
                "finance": {}}},
            "done": {"type": "final"}}}))
}

fn parallel() -> Machine {
    machine(&json!({"id": "p", "initial": "work", "states": {
        "work": {"type": "parallel", "states": {
            "a": {"initial": "a1", "states": {"a1": {}, "a2": {}}},
            "b": {"initial": "b1", "states": {"b1": {}, "b2": {}}}}}}}))
}

fn adopt(m: &Machine, ids: &[&str]) -> Snapshot {
    from_state_ids(m, ids, Value::Null, Status::Running, false, NOW).unwrap()
}

fn restore(m: &Machine, snapshot: &Snapshot, timers: RestartTimers) -> SyncInterpreter {
    let options = RestoreOptions { verify_hash: Some(true), restart_timers: timers };
    SyncInterpreter::from_snapshot(
        m.clone(),
        MachineLogic::dynamic(),
        snapshot,
        Arc::new(RealClock::new()),
        &options,
    )
    .unwrap()
}

fn invalid(error: &StoreError) -> &str {
    match error {
        StoreError::Engine(Error::InvalidConfig { message, .. }) => message,
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[test]
fn a_compound_state_without_a_child_enters_its_initial() {
    let m = order();
    let snap = adopt(&m, &["order.review"]);
    assert_eq!(snap.state_ids, ["order.review.legal.pending"]);
    assert_eq!(
        snap.configuration,
        ["order", "order.review", "order.review.legal", "order.review.legal.pending"]
    );
    assert_eq!(snap.value, json!({"review": {"legal": "pending"}}));
    assert_eq!(snap.version, 4);
    assert_eq!(snap.machine_id, "order");
    assert_eq!(snap.machine_version.as_deref(), Some("2.0"));
    assert_eq!(snap.machine_hash, m.structure_hash());
    assert_eq!(snap.machine_hash, "dcbcfaf61594c6ec", "the hash Python computes for this chart");
    assert_eq!(snap.status, Status::Running);
    assert!((snap.taken_at - NOW).abs() < f64::EPSILON);
    assert!(snap.deadlines.is_empty() && snap.pending_events.is_empty() && snap.history.is_empty());
}

#[test]
fn a_leaf_names_its_ancestors_and_ids_may_be_relative() {
    let m = order();
    let full = adopt(&m, &["order.review.legal.ok"]);
    assert_eq!(full.state_ids, ["order.review.legal.ok"]);
    assert_eq!(full.value, json!({"review": {"legal": "ok"}}));
    let relative = adopt(&m, &["review.legal.ok"]);
    assert_eq!(relative, full);
    let finance = adopt(&m, &["order.review.finance"]);
    assert_eq!(finance.value, json!({"review": "finance"}));
    assert_eq!(adopt(&m, &["cart"]).value, json!("cart"));
    assert_eq!(
        adopt(&m, &["order.review", "order.review.legal.ok"]),
        full,
        "an ancestor together with its own descendant is fine"
    );
}

#[test]
fn the_machine_root_alone_completes_to_the_initial_configuration() {
    let snap = adopt(&order(), &["order"]);
    assert_eq!(snap.state_ids, ["order.cart"]);
}

#[test]
fn every_region_of_a_parallel_state_is_entered() {
    let m = parallel();
    let snap = adopt(&m, &["p.work.a.a2"]);
    assert_eq!(snap.state_ids, ["p.work.a.a2", "p.work.b.b1"]);
    assert_eq!(snap.value, json!({"work": {"a": "a2", "b": "b1"}}));
    assert_eq!(snap.configuration.len(), 6);

    let both = adopt(&m, &["p.work.a.a2", "p.work.b.b2"]);
    assert_eq!(both.state_ids, ["p.work.a.a2", "p.work.b.b2"]);
    assert_eq!(adopt(&m, &["p.work"]).state_ids, ["p.work.a.a1", "p.work.b.b1"]);
}

#[test]
fn adopted_snapshots_restore_and_keep_running() {
    let m = order();
    for ids in [vec!["order.cart"], vec!["order.review"], vec!["order.review.legal.ok"]] {
        let snap = adopt(&m, &ids);
        let interp = restore(&m, &snap, RestartTimers::Parked);
        assert_eq!(interp.state_ids(), snap.state_ids, "{ids:?}");
        assert_eq!(interp.status(), Status::Running);
    }
    let mut interp = restore(&m, &adopt(&m, &["order.cart"]), RestartTimers::Parked);
    interp.send("PAY").unwrap();
    assert_eq!(interp.state_ids(), ["order.review.legal.pending"]);

    let p = parallel();
    let interp = restore(&p, &adopt(&p, &["p.work.b.b2"]), RestartTimers::Parked);
    assert_eq!(interp.state_ids(), ["p.work.a.a1", "p.work.b.b2"]);
}

#[test]
fn adoption_agrees_with_what_the_engine_writes_itself() {
    let m = order();
    let mut interp = SyncInterpreter::new(m.clone(), MachineLogic::dynamic());
    interp.start().unwrap();
    interp.send("PAY").unwrap();
    let real = interp.snapshot().unwrap();
    let adopted = adopt(&m, &["order.review"]);
    assert_eq!(adopted.state_ids, real.state_ids);
    assert_eq!(adopted.configuration, real.configuration);
    assert_eq!(adopted.value, real.value);
    assert_eq!(adopted.context, real.context);
    assert_eq!(adopted.machine_hash, real.machine_hash);
    assert_eq!(adopted.machine_version, real.machine_version);
    assert_eq!(adopted.status, real.status);
}

#[test]
fn a_final_state_can_be_adopted_as_done() {
    let m = order();
    let snap = from_state_ids(&m, &["order.done"], Value::Null, Status::Done, false, NOW).unwrap();
    assert_eq!(snap.status, Status::Done);
    assert_eq!(snap.state_ids, ["order.done"]);
    let interp = restore(&m, &snap, RestartTimers::Parked);
    assert_eq!(interp.status(), Status::Done);
}

#[test]
fn the_given_context_is_merged_over_the_initial_one() {
    let m = order();
    let given = json!({"items": 3, "note": "adopted"});
    let snap = from_state_ids(&m, &["cart"], given, Status::Running, false, NOW).unwrap();
    assert_eq!(snap.context, json!({"currency": "EUR", "items": 3, "note": "adopted"}));
    let plain = adopt(&m, &["cart"]);
    assert_eq!(plain.context, json!({"currency": "EUR", "items": 0}));
    let none = adopt(&parallel(), &["p.work"]);
    assert_eq!(none.context, json!({}), "a machine without context gets an empty object");
}

#[test]
fn timers_record_one_deadline_per_fixed_after_delay() {
    let m = machine(&json!({"id": "t", "initial": "wait", "states": {
        "wait": {"after": {"500": "go", "1000": "go"}},
        "go": {"after": {"BACKOFF": "wait"}}}}));
    let snap = from_state_ids(&m, &["t.wait"], Value::Null, Status::Running, true, NOW).unwrap();
    assert_eq!(snap.deadlines.len(), 2);
    let first = &snap.deadlines[0];
    assert_eq!(first.state_id, "t.wait");
    assert_eq!(first.event_type, "after.500.t.wait", "the Python spelling");
    assert_eq!(first.delay_ms, json!(500));
    assert!((first.due_at_wall - (NOW + 0.5)).abs() < 1e-9);
    assert_eq!(first.entry_seq, 2, "synthetic, 1-based, in state-id order (root first)");
    let second = &snap.deadlines[1];
    assert_eq!(second.event_type, "after.1000.t.wait");
    assert!((second.due_at_wall - (NOW + 1.0)).abs() < 1e-9);

    let without =
        from_state_ids(&m, &["t.wait"], Value::Null, Status::Running, false, NOW).unwrap();
    assert_eq!(without.deadlines.len(), 0);

    let named = from_state_ids(&m, &["t.go"], Value::Null, Status::Running, true, NOW).unwrap();
    assert_eq!(named.deadlines.len(), 0, "named delays need the logic registry and are skipped");
}

#[test]
fn adopted_deadlines_re_arm_on_restore() {
    let m = machine(&json!({"id": "t", "initial": "wait", "states": {
        "wait": {"after": {"500": "go", "1000": "go"}}, "go": {}}}));
    let now = xstate_persistence::store::wall_now();
    let snap = from_state_ids(&m, &["t.wait"], Value::Null, Status::Running, true, now).unwrap();
    let interp = restore(&m, &snap, RestartTimers::Resume);
    assert_eq!(interp.pending_timers(), 2);
    let parked = restore(&m, &snap, RestartTimers::Parked);
    assert_eq!(parked.pending_timers(), 0);
    // The deadlines survive the JSON round trip every store performs.
    let again = Snapshot::from_json(&snap.to_json().unwrap()).unwrap();
    assert_eq!(again.deadlines, snap.deadlines);
}

#[test]
fn the_snapshot_round_trips_through_json() {
    let snap = adopt(&order(), &["order.review.legal.ok"]);
    let again = Snapshot::from_json(&snap.to_json().unwrap()).unwrap();
    assert_eq!(again, snap);
}

#[test]
fn unknown_ids_are_state_not_found() {
    let err = from_state_ids(&order(), &["order.nope"], Value::Null, Status::Running, false, NOW)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::Engine(Error::StateNotFound { target, .. }) if target == "order.nope"),
        "{err:?}"
    );
    let relative = from_state_ids(&order(), &["nope"], Value::Null, Status::Running, false, NOW);
    assert!(matches!(relative, Err(StoreError::Engine(Error::StateNotFound { .. }))));
}

#[test]
fn history_states_cannot_be_adopted() {
    let h = machine(&json!({"id": "h", "initial": "x", "states": {
        "x": {"initial": "x1", "states": {"x1": {}, "x2": {}, "hist": {"type": "history"}}}}}));
    let err =
        from_state_ids(&h, &["h.x.hist"], Value::Null, Status::Running, false, NOW).unwrap_err();
    assert!(invalid(&err).contains("history"), "{err:?}");
    assert_eq!(adopt(&h, &["h.x"]).state_ids, ["h.x.x1"], "history children are not regions");
}

#[test]
fn illegal_combinations_are_rejected() {
    let m = order();
    let two_children = from_state_ids(
        &m,
        &["order.review.legal", "order.review.finance"],
        Value::Null,
        Status::Running,
        false,
        NOW,
    )
    .unwrap_err();
    assert!(invalid(&two_children).contains("exactly one child"), "{two_children:?}");

    let two_branches =
        from_state_ids(&m, &["order.cart", "order.done"], Value::Null, Status::Running, false, NOW)
            .unwrap_err();
    assert!(invalid(&two_branches).contains("exactly one child"), "{two_branches:?}");

    let two_leaves = from_state_ids(
        &m,
        &["order.review.legal.pending", "order.review.legal.ok"],
        Value::Null,
        Status::Running,
        false,
        NOW,
    )
    .unwrap_err();
    assert!(invalid(&two_leaves).contains("exactly one child"), "{two_leaves:?}");
}

#[test]
fn empty_ids_bad_status_and_bad_context_are_rejected() {
    let m = order();
    let none = from_state_ids(&m, &[], Value::Null, Status::Running, false, NOW).unwrap_err();
    assert!(invalid(&none).contains("no state ids"));
    for status in [Status::Error, Status::Stopped] {
        let err = from_state_ids(&m, &["cart"], Value::Null, status, false, NOW).unwrap_err();
        assert!(invalid(&err).contains("status"), "{status:?}");
    }
    let err = from_state_ids(&m, &["cart"], json!([1]), Status::Running, false, NOW).unwrap_err();
    assert!(invalid(&err).contains("context must be a JSON object"));
}

#[test]
fn machines_with_named_implementations_still_adopt() {
    let m = machine(&json!({"id": "a", "initial": "x", "states": {
        "x": {"entry": "greet", "on": {"GO": {"target": "y", "guard": "allowed"}}}, "y": {}}}));
    let snap = adopt(&m, &["a.y"]);
    assert_eq!(snap.state_ids, ["a.y"]);
}
