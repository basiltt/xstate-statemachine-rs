//! Snapshot migration: path finding, chained steps and the failure modes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_persistence::StoreError;
use xstate_persistence::migration::{MigrationError, MigrationStep, SnapshotMigrator};

fn step(f: impl Fn(Value) -> Value + Send + Sync + 'static) -> MigrationStep {
    Arc::new(move |blob| Ok(f(blob)))
}

/// A step that appends its own label to `context.trail`.
fn mark(label: &'static str) -> MigrationStep {
    step(move |mut blob| {
        let trail = blob["context"]["trail"].as_array().cloned().unwrap_or_default();
        let mut trail = trail;
        trail.push(json!(label));
        blob["context"]["trail"] = Value::Array(trail);
        blob
    })
}

fn blob(version: Option<&str>) -> Value {
    json!({
        "machine_id": "order",
        "machine_version": version,
        "machine_hash": "abc123",
        "state_ids": ["order.paying"],
        "configuration": ["order", "order.paying"],
        "context": {}
    })
}

fn hops(path: Option<Vec<(String, String)>>) -> Vec<String> {
    path.unwrap().into_iter().map(|(f, t)| format!("{f}>{t}")).collect()
}

fn chain() -> SnapshotMigrator {
    let mut m = SnapshotMigrator::new();
    m.register("1.0", "2.0", mark("1>2"), None).unwrap();
    m.register("2.0", "3.0", mark("2>3"), None).unwrap();
    m
}

#[test]
fn path_follows_registered_edges() {
    let m = chain();
    assert_eq!(hops(m.path("1.0", "3.0")), ["1.0>2.0", "2.0>3.0"]);
    assert_eq!(hops(m.path("1.0", "2.0")), ["1.0>2.0"]);
    assert_eq!(hops(m.path("2.0", "2.0")), Vec::<String>::new(), "already there");
    assert!(m.path("3.0", "1.0").is_none(), "steps are one-way");
    assert!(m.path("1.0", "9.0").is_none());
    assert!(m.can_migrate(None, Some("1.0"), Some("3.0")));
    assert!(!m.can_migrate(None, Some("3.0"), Some("1.0")));
    assert!(m.can_migrate(None, Some("x"), Some("x")), "same label needs no steps");
}

#[test]
fn path_prefers_the_shortest_route_then_the_earliest_registered() {
    let mut m = chain();
    m.register("1.0", "3.0", mark("1>3"), None).unwrap();
    assert_eq!(hops(m.path("1.0", "3.0")), ["1.0>3.0"], "the shortcut beats two hops");

    let mut tie = SnapshotMigrator::new();
    tie.register("a", "x", mark("a>x"), None).unwrap();
    tie.register("a", "y", mark("a>y"), None).unwrap();
    tie.register("x", "z", mark("x>z"), None).unwrap();
    tie.register("y", "z", mark("y>z"), None).unwrap();
    assert_eq!(hops(tie.path("a", "z")), ["a>x", "x>z"], "earliest-registered step wins ties");
}

#[test]
fn unlabelled_snapshots_only_migrate_to_unlabelled() {
    let m = chain();
    assert!(!m.can_migrate(Some("order"), None, Some("2.0")));
    assert!(!m.can_migrate(Some("order"), Some("1.0"), None));
    assert!(m.can_migrate(Some("order"), None, None));
}

#[test]
fn migrate_chains_steps_rewrites_the_version_and_drops_the_hash() {
    let out = chain().migrate(blob(Some("1.0")), Some("3.0"), None).unwrap();
    assert_eq!(out["machine_version"], "3.0");
    assert_eq!(out["context"]["trail"], json!(["1>2", "2>3"]), "steps ran in order");
    assert!(out.get("machine_hash").is_none(), "the structure changed by definition");
    assert_eq!(out["state_ids"], json!(["order.paying"]));
    assert_eq!(out["configuration"], json!(["order", "order.paying"]), "untouched ids keep it");
}

#[test]
fn migrate_to_the_current_version_is_a_no_op() {
    let original = blob(Some("2.0"));
    let out = chain().migrate(original.clone(), Some("2.0"), None).unwrap();
    assert_eq!(out, original, "no steps ran: the hash stays");
}

#[test]
fn each_step_sees_the_previous_hops_version() {
    let mut m = SnapshotMigrator::new();
    m.register(
        "1",
        "2",
        step(|b| {
            assert_eq!(b["machine_version"], "1");
            b
        }),
        None,
    )
    .unwrap();
    m.register(
        "2",
        "3",
        step(|b| {
            assert_eq!(b["machine_version"], "2");
            b
        }),
        None,
    )
    .unwrap();
    assert_eq!(m.migrate(blob(Some("1")), Some("3"), None).unwrap()["machine_version"], "3");
}

#[test]
fn a_step_that_renames_state_ids_leaves_configuration_to_be_rederived() {
    let mut m = SnapshotMigrator::new();
    m.register(
        "1.0",
        "2.0",
        step(|mut b| {
            b["state_ids"] = json!(["order.payment.card"]);
            b
        }),
        None,
    )
    .unwrap();
    let out = m.migrate(blob(Some("1.0")), Some("2.0"), None).unwrap();
    assert_eq!(out["state_ids"], json!(["order.payment.card"]));
    assert_eq!(out["configuration"], Value::Null);
}

#[test]
fn a_step_that_rewrites_both_fields_consistently_is_accepted() {
    let mut m = SnapshotMigrator::new();
    m.register(
        "1.0",
        "2.0",
        step(|mut b| {
            b["state_ids"] = json!(["order.payment.card"]);
            b["configuration"] = json!(["order", "order.payment", "order.payment.card"]);
            b
        }),
        None,
    )
    .unwrap();
    let out = m.migrate(blob(Some("1.0")), Some("2.0"), None).unwrap();
    assert_eq!(out["configuration"].as_array().unwrap().len(), 3);
}

#[test]
fn inconsistent_state_ids_and_configuration_are_refused() {
    let mut m = SnapshotMigrator::new();
    m.register(
        "1.0",
        "2.0",
        step(|mut b| {
            b["state_ids"] = json!(["order.elsewhere"]);
            b["configuration"] = json!(["order", "order.paying", "order.other"]);
            b
        }),
        None,
    )
    .unwrap();
    let err = m.migrate(blob(Some("1.0")), Some("2.0"), None).unwrap_err();
    assert!(
        matches!(&err, MigrationError::StepFailed { from, to, message, .. }
            if from == "1.0" && to == "2.0" && message.contains("order.elsewhere")),
        "{err}"
    );
}

#[test]
fn a_missing_route_is_no_path() {
    let err = chain().migrate(blob(Some("9.0")), Some("3.0"), None).unwrap_err();
    assert!(matches!(err, MigrationError::NoPath { ref machine_id, .. } if machine_id == "order"));
    assert_eq!(
        err.to_string(),
        r#"no migration path for machine 'order' from version "9.0" to "3.0""#
    );
    let unlabelled = chain().migrate(blob(None), Some("2.0"), None).unwrap_err();
    assert!(unlabelled.to_string().contains("from version None"), "{unlabelled}");
}

#[test]
fn failing_and_malformed_steps_are_typed_errors() {
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", Arc::new(|_| Err(StoreError::Other("bad data".into()))), None).unwrap();
    m.register("2", "3", step(|_| json!([1, 2])), None).unwrap();
    let failed = m.migrate(blob(Some("1")), Some("2"), None).unwrap_err();
    assert!(
        matches!(&failed, MigrationError::StepFailed { message, .. } if message.contains("bad data"))
    );
    let not_object = m.migrate(blob(Some("2")), Some("3"), None).unwrap_err();
    assert!(
        matches!(&not_object, MigrationError::StepFailed { message, .. } if message.contains("object"))
    );
}

#[test]
fn scoped_steps_win_over_unscoped_ones_for_their_machine() {
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", mark("any"), None).unwrap();
    m.register("1", "2", mark("order"), Some("order")).unwrap();
    m.register("2", "3", mark("invoice-only"), Some("invoice")).unwrap();

    let order = m.migrate(blob(Some("1")), Some("2"), None).unwrap();
    assert_eq!(order["context"]["trail"], json!(["order"]));
    let mut other = blob(Some("1"));
    other["machine_id"] = json!("cart");
    assert_eq!(m.migrate(other, Some("2"), None).unwrap()["context"]["trail"], json!(["any"]));

    assert_eq!(m.steps_for(Some("order")), [("1".to_owned(), "2".to_owned())]);
    assert_eq!(m.steps_for(Some("invoice")).len(), 2);
    assert_eq!(m.steps_for(None).len(), 1);
    assert!(m.can_migrate(Some("invoice"), Some("1"), Some("3")));
    assert!(!m.can_migrate(Some("order"), Some("1"), Some("3")));
    assert!(!m.can_migrate(None, Some("1"), Some("3")));
}

#[test]
fn an_explicit_machine_id_overrides_the_blobs() {
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", mark("cart"), Some("cart")).unwrap();
    assert!(m.migrate(blob(Some("1")), Some("2"), None).is_err());
    let out = m.migrate(blob(Some("1")), Some("2"), Some("cart")).unwrap();
    assert_eq!(out["context"]["trail"], json!(["cart"]));
}

#[test]
fn registering_a_hop_twice_replaces_the_step_and_a_no_op_hop_is_refused() {
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", mark("first"), None).unwrap();
    m.register("1", "2", mark("second"), None).unwrap();
    let out = m.migrate(blob(Some("1")), Some("2"), None).unwrap();
    assert_eq!(out["context"]["trail"], json!(["second"]), "last registration wins");
    assert!(matches!(
        m.register("1", "1", mark("x"), None),
        Err(MigrationError::SameVersion { version }) if version == "1"
    ));
    assert!(format!("{m:?}").contains("1->2"));
}

#[test]
fn numeric_version_labels_are_compared_as_text() {
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", mark("n"), None).unwrap();
    let mut numeric = blob(None);
    numeric["machine_version"] = json!(1);
    let out = m.migrate(numeric, Some("2"), None).unwrap();
    assert_eq!(out["machine_version"], "2");
}

#[test]
fn migration_errors_convert_into_store_errors() {
    let no_path = chain().migrate(blob(Some("9")), Some("3.0"), None).unwrap_err();
    assert!(matches!(StoreError::from(no_path), StoreError::Other(_)));
    let mut m = SnapshotMigrator::new();
    m.register("1", "2", step(|_| json!(1)), None).unwrap();
    let failed = m.migrate(blob(Some("1")), Some("2"), None).unwrap_err();
    assert!(matches!(StoreError::from(failed), StoreError::Corrupt(_)));
}
