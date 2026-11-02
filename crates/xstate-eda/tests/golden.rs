#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(not(feature = "sqlite"), allow(missing_docs))] // the whole file needs SQLite
#![cfg(feature = "sqlite")]
//! Goldens written by the Python library (`conformance/tools/eda_golden.py`): envelope JSON and
//! validation messages, dead-letter records, AsyncAPI documents, and SQLite files produced by
//! `SQLiteOutboxStore` / `SQLiteDeadLetterStore`.

mod common;

use serde_json::{Value, json};
use xstate_core::Machine;
use xstate_eda::{
    AsyncApiOptions, DEFAULT_CLAIM_LEASE_S, DeadLetter, DeadLetterStore, Envelope, OutboxStore,
    SqliteDeadLetterStore, SqliteOutboxStore, asyncapi_document, consumed_events,
    default_event_name, publish_specs, validate_asyncapi,
};
use xstate_persistence::DEFAULT_MAX_SNAPSHOT_BYTES;

#[test]
fn envelope_json_is_byte_identical() {
    let g = common::golden("envelopes.json");
    for case in ["full", "minimal"] {
        let dict = &g[case]["dict"];
        let env = Envelope::from_dict(dict).unwrap();
        assert_eq!(&Value::Object(env.to_dict()), dict, "{case}");
        assert_eq!(
            env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap(),
            g[case]["json"].as_str().unwrap(),
            "{case}"
        );
        let back =
            Envelope::from_json(g[case]["json"].as_str().unwrap(), DEFAULT_MAX_SNAPSHOT_BYTES)
                .unwrap();
        assert_eq!(back, env);
    }
    let full = Envelope::from_dict(&g["full"]["dict"]).unwrap();
    assert_eq!(full.attempt(), 2);
    assert_eq!(full.to_event(None).unwrap().ty, "PAY");
}

#[test]
fn from_dict_outcomes_and_messages_match_python() {
    let g = common::golden("envelopes.json");
    for case in g["from_dict"].as_array().unwrap() {
        let input = &case["input"];
        match Envelope::from_dict(input) {
            Ok(env) => {
                let mut got = env.to_dict();
                if input.get("id").is_none_or(|v| v.as_str() == Some("")) {
                    assert_eq!(got["id"].as_str().unwrap().len(), 36);
                    got.insert("id".into(), json!("<generated>"));
                }
                assert_eq!(Some(&Value::Object(got)), case.get("ok"), "input {input}");
            }
            Err(e) => assert_eq!(Some(&json!(e.to_string())), case.get("error"), "input {input}"),
        }
    }
    for (ty, expected) in g["default_event_name"].as_object().unwrap() {
        assert_eq!(default_event_name(ty), expected.as_str().unwrap(), "{ty}");
    }
}

#[test]
fn dead_letter_record_shape_matches_python() {
    let g = common::golden("dead_letters.json");
    let dict = &g["record"]["dict"];
    let rec = DeadLetter::from_dict(dict).unwrap();
    assert_eq!(&rec.to_dict(), dict);
    assert_eq!(rec.to_json(), g["record"]["json"].as_str().unwrap());
    let got = rec.to_dict();
    let keys: Vec<&String> = got.as_object().unwrap().keys().collect();
    let expected: Vec<&String> = dict.as_object().unwrap().keys().collect();
    assert_eq!(keys, expected, "field order");
}

fn options(raw: &Value) -> AsyncApiOptions {
    let text = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_owned);
    AsyncApiOptions {
        server: raw.get("server").and_then(Value::as_object).cloned(),
        inbound_topic: text("inbound_topic"),
        outbound_topic: text("outbound_topic").unwrap_or_else(|| "events".into()),
        title: text("title"),
    }
}

#[test]
fn asyncapi_documents_match_python() {
    for case in common::golden("asyncapi.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let machine = Machine::from_value(&case["config"]).unwrap();
        let doc = asyncapi_document(&machine, &options(&case["options"])).unwrap();
        assert_eq!(doc, case["document"], "{name}");
        assert_eq!(
            serde_json::to_string(&doc).unwrap(),
            serde_json::to_string(&case["document"]).unwrap(),
            "{name}: key order"
        );
        validate_asyncapi(&doc).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(json!(consumed_events(&machine)), case["consumed"], "{name}");
        let specs: Vec<Value> = publish_specs(&machine)
            .unwrap()
            .iter()
            .map(xstate_eda::PublishSpec::to_value)
            .collect();
        assert_eq!(Value::Array(specs), case["publish_specs"], "{name}");
    }
}

#[test]
fn malformed_publish_meta_is_refused() {
    let machine = Machine::from_value(&json!({
        "id": "m", "initial": "a",
        "states": {"a": {"on": {"GO": {"target": "a", "meta": {"publish": {"type": "x", "data": "total"}}}}}}
    }))
    .unwrap();
    let err = publish_specs(&machine).unwrap_err();
    assert_eq!(err.to_string(), "meta.publish.data must be a list of context field names");
    assert!(asyncapi_document(&machine, &AsyncApiOptions::default()).is_err());
}

/// Copy a golden database so the test never modifies the checked-in file.
fn copy_golden(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::copy(common::golden_dir().join(name), &path).unwrap();
    (dir, path)
}

#[test]
fn python_outbox_file_is_readable() {
    let (_dir, path) = copy_golden("outbox_py.sqlite");
    let store = SqliteOutboxStore::open(&path).unwrap();
    assert_eq!(store.count(false).unwrap(), 3);
    let pending = store.pending(10).unwrap();
    let seqs: Vec<i64> = pending.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, [2, 3], "row 1 was marked sent by Python");
    assert_eq!(pending[0].topic, "orders");
    assert_eq!(pending[0].envelope.data, json!({"n": 1, "note": "日本"}));
    assert_eq!(pending[0].envelope.subject.as_deref(), Some("o-1"));
    // Python's relay holds a one-hour lease on row 2: another relay only gets row 3
    let claimed = store.claim(10, "rust-relay", DEFAULT_CLAIM_LEASE_S).unwrap().unwrap();
    assert_eq!(claimed.iter().map(|r| r.seq).collect::<Vec<_>>(), [3]);
    assert_eq!(store.mark_sent(&[3]).unwrap(), 1);
    let env = Envelope::new("order.paid").subject("o-9").build().unwrap();
    store.add("orders", &env).unwrap();
    assert_eq!(store.count(true).unwrap(), 2);
}

#[test]
fn python_dead_letter_file_is_readable() {
    let (_dir, path) = copy_golden("dlq_py.sqlite");
    let store = SqliteDeadLetterStore::open(&path).unwrap();
    assert_eq!(store.count().unwrap(), 1);
    let rec = store.get("env-1").unwrap().unwrap();
    assert_eq!(rec.reason, "max_attempts");
    assert_eq!(rec.attempts, Some(5));
    assert_eq!(rec.event["payload"], json!({"password": "***", "n": 1}));
    assert_eq!(rec.errors[0].ty, "ProcessingFailedError");
    assert_eq!(rec.machine_version.as_deref(), Some("1.2.0"));
    assert_eq!(store.list(false, 10).unwrap().len(), 1);
    let log = store.audit_log().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].action.as_str(), log[0].actor.as_str()), ("purge", "ops"));
    assert_eq!(log[0].detail, json!({"n": 1}));
    let env = Envelope::from_dict(rec.envelope.as_ref().unwrap()).unwrap();
    assert_eq!(env.subject.as_deref(), Some("o-1"));
}
