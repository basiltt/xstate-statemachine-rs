#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `DeadLetterPlugin` on a real interpreter, the stores and the broker sink.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_core::redact::RedactKeys;
use xstate_core::{Machine, MachineLogic, Observer, SyncInterpreter};
use xstate_eda::{
    BrokerDeadLetterSink, DeadLetter, DeadLetterPlugin, DeadLetterSink, DeadLetterStore, EdaError,
    MemoryDeadLetterStore, SyncFakeBrokerAdapter, dlq_topic, redact_record, sink_fn,
};

fn payment() -> Machine {
    Machine::from_value(&json!({
        "id": "pay", "initial": "idle",
        "context": {"attempt": 2, "card_token": "tok_live_secret", "n": 1},
        "states": {
            "idle": {"on": {"PAY": "charging", "GIVE_UP": "dead"}},
            "charging": {"invoke": {"src": "charge", "onError": "dead", "onDone": "paid"}},
            "paid": {"type": "final"},
            "dead": {"tags": ["dead-letter"], "type": "final"}
        }
    }))
    .unwrap()
}

fn logic(fail: bool) -> MachineLogic<Value> {
    MachineLogic::dynamic().service("charge", move |_, _| {
        if fail {
            Err(xstate_core::Error::logic("declined tok_live_secret"))
        } else {
            Ok(json!({"ok": true}))
        }
    })
}

fn run(plugin: &Arc<DeadLetterPlugin>, fail: bool, event: &str) {
    let mut interp = SyncInterpreter::new(payment(), logic(fail));
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send(event).unwrap();
}

#[test]
fn a_failed_service_reaching_a_dead_letter_state_is_captured() {
    let store = Arc::new(MemoryDeadLetterStore::new());
    let plugin = Arc::new(DeadLetterPlugin::new(store.clone()).instance_id("pay-1"));
    run(&plugin, true, "PAY");
    let records = store.all();
    assert_eq!(records.len(), 1, "{records:#?}");
    let rec = &records[0];
    assert_eq!(rec.machine_id, "pay-1");
    assert_eq!(rec.state_id, "pay.dead");
    assert_eq!(rec.reason, "dead_letter_state");
    assert_eq!(rec.attempts, Some(2));
    assert_eq!(rec.errors.len(), 1, "{:?}", rec.errors);
    assert_eq!((rec.errors[0].source.as_str(), rec.errors[0].name.as_str()), ("service", "charge"));
    assert!(rec.errors[0].message.contains("declined"), "{:?}", rec.errors);
    assert!(
        !rec.errors[0].message.contains("tok_live_secret"),
        "secret scrubbed: {:?}",
        rec.errors
    );
    assert_eq!(rec.snapshot["context"]["card_token"], "***");
    assert_eq!(rec.snapshot["context"]["n"], 1);
    assert_eq!(rec.machine_hash.as_deref(), Some(payment().structure_hash()));
    assert_eq!(rec.id.len(), 32);
}

#[test]
fn a_clean_path_writes_nothing_and_explicit_state_ids_win() {
    let store = Arc::new(MemoryDeadLetterStore::new());
    let plugin = Arc::new(DeadLetterPlugin::new(store.clone()));
    run(&plugin, false, "PAY");
    assert_eq!(store.len(), 0);

    let explicit = Arc::new(
        DeadLetterPlugin::new(store.clone()).state_ids(["pay.paid"]).include_snapshot(false),
    );
    run(&explicit, false, "PAY");
    let recs = store.all();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].state_id, "pay.paid");
    assert_eq!(recs[0].snapshot, json!({}));
    assert!(DeadLetterPlugin::new(store).max_errors(0).is_err());
}

#[test]
fn a_failing_sink_never_stops_the_machine() {
    let plugin = Arc::new(DeadLetterPlugin::new(Arc::new(sink_fn(|_| {
        Err(EdaError::InvalidArgument("sink down".into()))
    }))));
    let mut interp = SyncInterpreter::new(payment(), logic(true));
    interp.observe(plugin as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GIVE_UP").unwrap();
    assert_eq!(interp.state_ids(), ["pay.dead"]);
}

#[test]
fn broker_sink_publishes_to_the_dlq_topic_and_stores_first() {
    let store = Arc::new(MemoryDeadLetterStore::new());
    let sink =
        BrokerDeadLetterSink::new(SyncFakeBrokerAdapter::new(), "orders").with_store(store.clone());
    assert_eq!(sink.topic(), "orders.dlq");
    assert_eq!(dlq_topic("x"), "x.dlq");
    let mut rec =
        DeadLetter::new("order", "", json!({"type": "PAY", "payload": {"token": "t"}}), 5.0);
    rec.envelope = Some(json!({"subject": "o-1", "id": "e"}));
    rec.machine_version = Some("2".into());
    sink.put(rec.clone()).unwrap();
    assert!(store.get(&rec.id).unwrap().is_some());
    let published = sink.broker().published_on("orders.dlq");
    assert_eq!(published.len(), 1);
    let env = &published[0];
    assert_eq!(env.ty, "xsm.deadletter");
    assert_eq!(env.subject.as_deref(), Some("o-1"));
    assert_eq!(env.source, "xsm/order");
    assert_eq!(env.machineversion.as_deref(), Some("2"));
    assert_eq!(env.data["event"]["payload"]["token"], "***");
    assert_eq!(env.data["id"], json!(rec.id));
}

#[test]
fn redact_record_masks_event_snapshot_and_envelope() {
    let mut rec =
        DeadLetter::new("m", "s", json!({"type": "E", "payload": {"password": "x"}}), 1.0);
    rec.snapshot = json!({"context": {"api_key": "k"}});
    rec.envelope = Some(json!({"data": {"email": "a@b"}}));
    let out = redact_record(&rec, &RedactKeys::default());
    assert_eq!(out.event["payload"]["password"], "***");
    assert_eq!(out.snapshot["context"]["api_key"], "***");
    assert_eq!(out.envelope.unwrap()["data"]["email"], "***");
    assert_eq!(rec.event["payload"]["password"], "x", "input untouched");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_store_lists_oldest_first_and_purges() {
    let dir = tempfile::tempdir().unwrap();
    let store = xstate_eda::SqliteDeadLetterStore::open(dir.path().join("dlq.sqlite")).unwrap();
    for (id, at) in [("b", 2.0), ("a", 2.0), ("c", 1.0)] {
        let mut rec = DeadLetter::new("m", "", json!({"type": "E", "payload": {}}), at);
        rec.id = id.into();
        store.put(rec).unwrap();
    }
    let ids: Vec<String> = store.list(true, 10).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["c", "a", "b"]);
    assert!(store.purge_older_than(f64::NAN).is_err());
    assert_eq!(store.purge_older_than(1.5).unwrap(), 1);
    assert_eq!(store.count().unwrap(), 2);
}
