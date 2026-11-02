//! End-to-end tests of `xsm snapshots` against real `SQLite` and file stores.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::{fixture, run_json, xsm};
use predicates::prelude::*;
use serde_json::Value;
use xstate_core::graph::{GuardPolicy, stub_logic};
use xstate_core::{DynContext, Machine, SimulatedClock, SyncInterpreter};
use xstate_persistence::{FileStore, SaveOptions, SqliteStore, StateStore};

fn machine_from_fixture(name: &str) -> Machine {
    Machine::from_json(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
}

/// A snapshot of `machine` after `events`, as JSON text.
fn snapshot_json(machine: &Machine, events: &[&str]) -> String {
    let logic = stub_logic::<DynContext>(machine, GuardPolicy::True);
    let mut interp = SyncInterpreter::with_clock(
        machine.clone(),
        logic,
        std::sync::Arc::new(SimulatedClock::new()),
    );
    interp.start().unwrap();
    for event in events {
        interp.send(*event).unwrap();
    }
    interp.snapshot_json().unwrap()
}

fn save(store: &dyn StateStore, key: &str, machine: &Machine, events: &[&str]) {
    let label = machine.version().unwrap_or("");
    let options = SaveOptions { machine_version: label, ..SaveOptions::default() };
    store.save(key, &snapshot_json(machine, events), &options).unwrap();
}

fn url_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

struct Stores {
    _dir: tempfile::TempDir,
    sqlite_url: String,
    file_url: String,
}

/// A SQLite store and a file store, each holding two checkout instances.
fn stores_with_checkouts() -> Stores {
    let dir = tempfile::tempdir().unwrap();
    let machine = machine_from_fixture("hierarchical.json");
    let db = dir.path().join("snapshots.db");
    let sqlite = SqliteStore::open(&db).unwrap();
    save(&sqlite, "order:1", &machine, &[]);
    save(&sqlite, "order:2", &machine, &["PAY"]);
    drop(sqlite);
    let files = dir.path().join("files");
    let file_store = FileStore::open(&files).unwrap();
    save(&file_store, "order:1", &machine, &[]);
    save(&file_store, "order:2", &machine, &["PAY"]);
    Stores {
        sqlite_url: format!("sqlite:///{}", url_path(&db)),
        file_url: format!("file:///{}", url_path(&files)),
        _dir: dir,
    }
}

#[test]
fn snapshots_lists_keys_in_a_sqlite_store() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--store", &stores.sqlite_url])
        .assert()
        .success()
        .stdout(predicate::str::contains("snapshots (2)"))
        .stdout(predicate::str::contains("order:1"))
        .stdout(predicate::str::contains("order:2"))
        .stdout(predicate::str::contains("2.1"))
        .stdout(predicate::str::contains("waiting"))
        .stdout(predicate::str::contains("secret-123").not());
}

#[test]
fn snapshots_lists_keys_in_a_file_store() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--store", &stores.file_url])
        .assert()
        .success()
        .stdout(predicate::str::contains("order:1"))
        .stdout(predicate::str::contains("order:2"));
}

#[test]
fn snapshots_json_never_contains_the_context() {
    let stores = stores_with_checkouts();
    let mut cmd = xsm();
    cmd.args(["snapshots", "--json", "--store", &stores.sqlite_url]);
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!text.contains("secret-123"), "context leaked: {text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["count"], 2);
    assert_eq!(value["truncated"], false);
    let first = &value["snapshots"][0];
    assert_eq!(first["key"], "order:1");
    assert_eq!(first["version"], 1);
    assert_eq!(first["machine_version"], "2.1");
    assert_eq!(first["status"], "running");
    assert_eq!(first["state_ids"], serde_json::json!(["checkout.cart"]));
    assert!(first["age_s"].as_f64().unwrap() >= 0.0);
    assert!(first.get("context").is_none());
}

#[test]
fn snapshots_filters_by_prefix_and_limit() {
    let stores = stores_with_checkouts();
    let by_prefix = run_json(xsm().args([
        "snapshots",
        "--json",
        "--prefix",
        "order:2",
        "--store",
        &stores.sqlite_url,
    ]));
    assert_eq!(by_prefix["count"], 1);
    let limited = run_json(xsm().args([
        "snapshots",
        "--json",
        "--limit",
        "1",
        "--store",
        &stores.sqlite_url,
    ]));
    assert_eq!(limited["count"], 1);
    assert_eq!(limited["total"], 2);
    assert_eq!(limited["truncated"], true);
}

/// The checkout machine, edited by `edit`, written to a temp file.
fn edited_machine(dir: &Path, name: &str, edit: impl FnOnce(&mut Value)) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut doc: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("hierarchical.json")).unwrap())
            .unwrap();
    edit(&mut doc);
    std::fs::write(&path, doc.to_string()).unwrap();
    path
}

/// The deployed machine gained a transition (new structure hash) but kept its version label.
fn restructured(dir: &Path) -> std::path::PathBuf {
    edited_machine(dir, "checkout-restructured.json", |doc| {
        doc["states"]["cart"]["on"]["CANCEL"] = serde_json::json!("receipt");
    })
}

/// The deployed machine was relabelled but its structure is unchanged.
fn relabelled(dir: &Path) -> std::path::PathBuf {
    edited_machine(dir, "checkout-v3.json", |doc| doc["version"] = serde_json::json!("3.0"))
}

#[test]
fn snapshots_detects_stale_records_by_version_label_like_python() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    let changed = relabelled(dir.path());

    for url in [&stores.sqlite_url, &stores.file_url] {
        xsm()
            .args(["snapshots", "--stale", "--store", url])
            .arg(&changed)
            .assert()
            .success()
            .stdout(predicate::str::contains("stale snapshots (2)"))
            .stdout(predicate::str::contains("3.0").not())
            .stdout(predicate::str::contains("2.1"));
        xsm()
            .args(["snapshots", "--fail-if-stale", "--store", url])
            .arg(&changed)
            .assert()
            .code(1)
            .stdout(predicate::str::contains("order:1"));
        // The unchanged machine carries the same label as every record: nothing is stale.
        xsm()
            .args(["snapshots", "--fail-if-stale", "--store", url])
            .arg(fixture("hierarchical.json"))
            .assert()
            .success()
            .stdout(predicate::str::contains("no stale snapshots"));
    }
}

#[test]
fn snapshots_default_ignores_a_structure_change_that_kept_the_label() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    let changed = restructured(dir.path());
    // Python's rule is the label only, so a structural edit without a version bump is not stale
    // by default ...
    xsm()
        .args(["snapshots", "--fail-if-stale", "--store", &stores.sqlite_url])
        .arg(&changed)
        .assert()
        .success()
        .stdout(predicate::str::contains("no stale snapshots"));
    // ... but the hash column still shows that restoring would be refused.
    xsm()
        .args(["snapshots", "--store", &stores.sqlite_url])
        .arg(&changed)
        .assert()
        .success()
        .stdout(predicate::str::contains("Hash"))
        .stdout(predicate::str::contains("MISMATCH"));
}

#[test]
fn snapshots_stale_by_hash_flags_structure_changes_only() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    let restructured = restructured(dir.path());
    let relabelled = relabelled(dir.path());

    for url in [&stores.sqlite_url, &stores.file_url] {
        xsm()
            .args(["snapshots", "--stale", "--stale-by", "hash", "--store", url])
            .arg(&restructured)
            .assert()
            .success()
            .stdout(predicate::str::contains("stale snapshots (2)"))
            .stdout(predicate::str::contains("MISMATCH"));
        xsm()
            .args(["snapshots", "--fail-if-stale", "--stale-by", "hash", "--store", url])
            .arg(&restructured)
            .assert()
            .code(1);
        // A relabel alone does not change the structure hash.
        xsm()
            .args(["snapshots", "--fail-if-stale", "--stale-by", "hash", "--store", url])
            .arg(&relabelled)
            .assert()
            .success();
    }
}

#[test]
fn snapshots_stale_by_either_flags_both_kinds_of_change() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    for changed in [restructured(dir.path()), relabelled(dir.path())] {
        xsm()
            .args(["snapshots", "--fail-if-stale", "--stale-by", "either"])
            .args(["--store", &stores.sqlite_url])
            .arg(&changed)
            .assert()
            .code(1);
    }
    xsm()
        .args(["snapshots", "--fail-if-stale", "--stale-by", "either"])
        .args(["--store", &stores.sqlite_url])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success();
}

#[test]
fn snapshots_never_calls_records_stale_when_the_machine_has_no_version() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    let unversioned = edited_machine(dir.path(), "unversioned.json", |doc| {
        doc.as_object_mut().unwrap().remove("version");
    });
    xsm()
        .args(["snapshots", "--fail-if-stale", "--store", &stores.sqlite_url])
        .arg(&unversioned)
        .assert()
        .success();
}

#[test]
fn snapshots_marks_every_record_when_a_machine_is_given_without_stale() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--store", &stores.sqlite_url])
        .arg(fixture("hierarchical.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("Hash"))
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn snapshots_ignores_records_of_other_machines_when_judging_staleness() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--fail-if-stale", "--store", &stores.sqlite_url])
        .arg(fixture("toggle.json"))
        .assert()
        .success();
}

#[test]
fn snapshots_json_json_reports_stale_flags() {
    let stores = stores_with_checkouts();
    let value = run_json(
        xsm()
            .args(["snapshots", "--json", "--store", &stores.sqlite_url])
            .arg(fixture("hierarchical.json")),
    );
    assert_eq!(value["machine"], "checkout");
    assert_eq!(value["machine_version"], "2.1");
    assert_eq!(value["stale_by"], "version");
    assert_eq!(value["snapshots"][0]["stale"], false);
    assert_eq!(value["snapshots"][0]["version_mismatch"], false);
    assert_eq!(value["snapshots"][0]["hash_mismatch"], false);
}

#[test]
fn snapshots_json_reports_each_kind_of_mismatch_separately() {
    let stores = stores_with_checkouts();
    let dir = tempfile::tempdir().unwrap();
    let value = run_json(
        xsm()
            .args(["snapshots", "--json", "--store", &stores.sqlite_url])
            .arg(restructured(dir.path())),
    );
    let row = &value["snapshots"][0];
    assert_eq!(row["stale"], false, "the default rule is the label");
    assert_eq!(row["version_mismatch"], false);
    assert_eq!(row["hash_mismatch"], true);
    let value = run_json(
        xsm()
            .args(["snapshots", "--json", "--stale-by", "either", "--store", &stores.sqlite_url])
            .arg(restructured(dir.path())),
    );
    assert_eq!(value["snapshots"][0]["stale"], true);
}

#[test]
fn snapshots_refuses_bad_stores_with_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let missing_db = format!("sqlite:///{}", url_path(&dir.path().join("missing.db")));
    let missing_dir = format!("file:///{}", url_path(&dir.path().join("missing")));
    xsm().args(["snapshots", "--store", &missing_db]).assert().code(2);
    xsm().args(["snapshots", "--store", &missing_dir]).assert().code(2);
    assert!(!dir.path().join("missing.db").exists(), "a typo must not create a database");
    assert!(!dir.path().join("missing").exists(), "a typo must not create a directory");
    xsm().args(["snapshots", "--store", "memory://"]).assert().code(2);
    xsm().args(["snapshots", "--store", "redis://localhost"]).assert().code(2);
    xsm().args(["snapshots", "--store", "plain-path"]).assert().code(2);
    let not_a_db = dir.path().join("notes.db");
    std::fs::write(&not_a_db, "this is not sqlite").unwrap();
    xsm()
        .args(["snapshots", "--store", &format!("sqlite:///{}", url_path(&not_a_db))])
        .assert()
        .code(2);
}

#[test]
fn snapshots_stale_needs_the_machine_file() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--stale", "--store", &stores.sqlite_url])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs the machine JSON"));
    xsm().args(["snapshots", "--fail-if-stale", "--store", &stores.sqlite_url]).assert().code(2);
}

#[test]
fn snapshots_rejects_an_unreadable_machine_file() {
    let stores = stores_with_checkouts();
    xsm()
        .args(["snapshots", "--stale", "--store", &stores.sqlite_url])
        .arg(fixture("invalid.json"))
        .assert()
        .code(2);
}

#[test]
fn snapshots_reports_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    drop(FileStore::open(&empty).unwrap());
    xsm()
        .args(["snapshots", "--store", &format!("file:///{}", url_path(&empty))])
        .assert()
        .success()
        .stdout(predicate::str::contains("store is empty"));
}
