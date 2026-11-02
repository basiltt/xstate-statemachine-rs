#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(not(feature = "sqlite"), allow(missing_docs))] // the whole file needs SQLite
#![cfg(feature = "sqlite")]
//! Live cross-language checks: SQLite outbox / dead-letter files written by Rust are read by
//! the Python library and vice versa. Skipped when the Python checkout's virtualenv is not
//! available (override the interpreter with `XSM_PYTHON`). The checked-in goldens
//! (`tests/golden.rs`) cover the Python → Rust direction without Python.

mod common;

use serde_json::{Value, json};
use xstate_eda::{
    DeadLetter, DeadLetterSink, DeadLetterStore, Envelope, ErrorEntry, OutboxStore,
    SqliteDeadLetterStore, SqliteOutboxStore,
};
use xstate_persistence::DEFAULT_MAX_SNAPSHOT_BYTES;

const PY_READ_OUTBOX: &str = r#"
import sys, json
from xstate_statemachine.eda import SQLiteOutboxStore
store = SQLiteOutboxStore(sys.argv[1])
rows = store.pending(limit=10)
claimed = store.claim(limit=10, owner="py", lease_s=30.0)
print(json.dumps({
    "pending": [[r.seq, r.topic, r.envelope.to_dict(), r.envelope.to_json()] for r in rows],
    "claimed": [r.seq for r in claimed],
    "count": store.count(),
}))
store.close()
"#;

const PY_WRITE_OUTBOX: &str = r#"
import sys
from xstate_statemachine.eda import Envelope, SQLiteOutboxStore
store = SQLiteOutboxStore(sys.argv[1])
store.add("py-topic", Envelope.new(type="order.paid", subject="py-1", data={"x": "é"}))
store.close()
"#;

const PY_READ_DLQ: &str = r#"
import sys, json
from xstate_statemachine.eda import SQLiteDeadLetterStore
store = SQLiteDeadLetterStore(sys.argv[1])
rec = store.get(sys.argv[2])
print(json.dumps({"record": rec.to_dict(), "audit": store.audit_log(), "count": len(store)}))
store.close()
"#;

fn rust_envelope(n: i64) -> Envelope {
    Envelope::new("order.paid")
        .subject(format!("o-{n}"))
        .data(json!({"n": n, "note": "日本 😀"}))
        .extension("tenant", "t1")
        .build()
        .unwrap()
}

#[test]
fn python_reads_a_rust_outbox_and_rust_reads_its_rows_back() {
    let Some(py) = common::python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outbox.sqlite");
    let store = SqliteOutboxStore::open(&path).unwrap();
    let envs: Vec<Envelope> = (0..3).map(rust_envelope).collect();
    for e in &envs {
        store.add("orders", e).unwrap();
    }
    store.mark_sent(&[1]).unwrap();
    drop(store);

    let out: Value =
        serde_json::from_str(&common::run_python(&py, PY_READ_OUTBOX, &[path.to_str().unwrap()]))
            .unwrap();
    assert_eq!(out["count"], 3);
    assert_eq!(out["claimed"], json!([2, 3]));
    let pending = out["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 2);
    for (row, env) in pending.iter().zip(&envs[1..]) {
        assert_eq!(row[1], "orders");
        assert_eq!(row[2], Value::Object(env.to_dict()));
        assert_eq!(row[3].as_str().unwrap(), env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap());
    }

    common::run_python(&py, PY_WRITE_OUTBOX, &[path.to_str().unwrap()]);
    let store = SqliteOutboxStore::open(&path).unwrap();
    let rows = store.pending(10).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].topic, "py-topic");
    assert_eq!(rows[2].envelope.data, json!({"x": "é"}));
    // Python's lease on rows 2 and 3 is live: a Rust relay only gets the Python row
    let claimed = store.claim(10, "rust", 30.0).unwrap().unwrap();
    assert_eq!(claimed.iter().map(|r| r.seq).collect::<Vec<_>>(), [4]);
}

#[test]
fn python_reads_a_rust_dead_letter_store() {
    let Some(py) = common::python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dlq.sqlite");
    let store = SqliteDeadLetterStore::open(&path).unwrap();
    let mut rec = DeadLetter::new(
        "order",
        "",
        json!({"type": "xsm.order.PAY", "payload": {"password": "p", "n": 1}}),
        1_700_000_000.5,
    );
    rec.attempts = Some(5);
    rec.errors =
        vec![ErrorEntry::new("dispatcher", "max_attempts", "ProcessingFailedError", "boom é")];
    rec.reason = "max_attempts".into();
    rec.envelope = Some(Value::Object(rust_envelope(1).to_dict()));
    rec.topic = Some("orders".into());
    rec.machine_hash = Some("abcdef0123456789".into());
    store.put(rec.clone()).unwrap();
    store
        .audit(
            "replay",
            Some(&rec.id),
            "fixed",
            &json!({"outcome": "processed", "token": "t"}),
            Some("ops"),
        )
        .unwrap();
    drop(store);

    let out: Value = serde_json::from_str(&common::run_python(
        &py,
        PY_READ_DLQ,
        &[path.to_str().unwrap(), &rec.id],
    ))
    .unwrap();
    assert_eq!(out["count"], 1);
    let mut expected = rec.to_dict();
    expected["event"]["payload"]["password"] = json!("***");
    assert_eq!(out["record"], expected);
    let audit = &out["audit"][0];
    assert_eq!((audit["action"].as_str(), audit["actor"].as_str()), (Some("replay"), Some("ops")));
    assert_eq!(audit["detail"], json!({"outcome": "processed", "token": "***"}));
    let reread = SqliteDeadLetterStore::open(&path).unwrap();
    assert_eq!(reread.get(&rec.id).unwrap().unwrap().to_dict(), expected);
    let _: &dyn DeadLetterSink = &reread;
}
