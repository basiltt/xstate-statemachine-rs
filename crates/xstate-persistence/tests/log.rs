//! Transition log: record JSON, the three stores, redaction and the observer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Map, Value, json};
use xstate_core::{Machine, MachineLogic, SimulatedClock, SyncInterpreter};
use xstate_persistence::StoreError;
#[cfg(feature = "sqlite")]
use xstate_persistence::log::SqliteLog;
use xstate_persistence::log::{
    AuditKeys, DEFAULT_REDACT_KEYS, Disposition, JsonLinesLog, MemoryLog, Origin,
    TransitionLogObserver, TransitionLogStore, TransitionRecord, redact,
};

/// Python: `json.dumps(TransitionRecord(...).to_dict(), sort_keys=True, default=str)`.
const GOLDEN_TRANSITION: &str = r#"{"actions": ["charge", "notify"], "actor": "alice", "correlation_id": "c-9", "disposition": "transition", "engine": false, "error": null, "event_payload": {"a": {"y": null, "z": [1, 2.5]}, "b": 1}, "event_type": "PAY", "from_states": ["m.a"], "machine_id": "order:1", "machine_version": "1.0", "origin": "external", "reason": null, "seq": 3, "to_states": ["m.b"], "ts": 1700000000.5}"#;
const GOLDEN_ENGINE: &str = r#"{"actions": [], "actor": null, "correlation_id": null, "disposition": "error", "engine": true, "error": {"message": "boom", "type": "ActionError"}, "event_payload": {"data": {"ok": true}, "kind": "done", "src": "fetch"}, "event_type": "done.invoke.fetch", "from_states": ["m.a"], "machine_id": "x", "machine_version": "", "origin": "internal", "reason": null, "seq": 1, "to_states": ["m.a"], "ts": 5.0}"#;

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn record(machine_id: &str, seq: u64, ts: f64) -> TransitionRecord {
    TransitionRecord {
        machine_id: machine_id.into(),
        seq,
        ts,
        event_type: "GO".into(),
        event_payload: Map::new(),
        from_states: strings(&["m.a"]),
        to_states: strings(&["m.b"]),
        actions: Vec::new(),
        disposition: Disposition::Transition,
        actor: None,
        reason: None,
        correlation_id: None,
        machine_version: String::new(),
        engine: false,
        error: None,
        origin: Origin::External,
    }
}

fn golden_transition() -> TransitionRecord {
    TransitionRecord {
        machine_id: "order:1".into(),
        seq: 3,
        ts: 1_700_000_000.5,
        event_type: "PAY".into(),
        event_payload: object(&json!({"b": 1, "a": {"z": [1, 2.5], "y": null}})),
        from_states: strings(&["m.a"]),
        to_states: strings(&["m.b"]),
        actions: strings(&["charge", "notify"]),
        actor: Some("alice".into()),
        correlation_id: Some("c-9".into()),
        machine_version: "1.0".into(),
        ..record("order:1", 3, 0.0)
    }
}

// ---------------------------------------------------------------------------------- record

#[test]
fn record_json_matches_python_goldens_both_ways() {
    assert_eq!(golden_transition().to_json(), GOLDEN_TRANSITION);
    assert_eq!(TransitionRecord::from_json(GOLDEN_TRANSITION).unwrap(), golden_transition());

    let engine = TransitionRecord {
        machine_id: "x".into(),
        seq: 1,
        ts: 5.0,
        event_type: "done.invoke.fetch".into(),
        event_payload: object(&json!({"kind": "done", "src": "fetch", "data": {"ok": true}})),
        from_states: strings(&["m.a"]),
        to_states: strings(&["m.a"]),
        disposition: Disposition::Error,
        engine: true,
        error: Some(json!({"type": "ActionError", "message": "boom"})),
        origin: Origin::Internal,
        ..record("x", 1, 5.0)
    };
    assert_eq!(engine.to_json(), GOLDEN_ENGINE);
    assert_eq!(TransitionRecord::from_json(GOLDEN_ENGINE).unwrap(), engine);
}

#[test]
fn records_written_before_origin_existed_read_as_external() {
    let old = r#"{"machine_id": "m", "seq": 1, "ts": 1, "event_type": "E"}"#;
    let rec = TransitionRecord::from_json(old).unwrap();
    assert_eq!(rec.origin, Origin::External);
    assert_eq!(rec.disposition, Disposition::Transition);
    assert!(rec.from_states.is_empty() && rec.event_payload.is_empty() && !rec.engine);
}

#[test]
fn bad_records_are_corrupt() {
    let ok = json!({"machine_id": "m", "seq": 1, "ts": 1.0, "event_type": "E"});
    TransitionRecord::from_value(&ok).unwrap();
    let mutate = |key: &str, value: Value| {
        let mut doc = ok.clone();
        doc[key] = value;
        doc
    };
    for bad in [
        json!([]),
        mutate("seq", json!(0)),
        mutate("seq", json!(-1)),
        mutate("seq", json!(1.5)),
        mutate("seq", json!(true)),
        mutate("ts", json!("now")),
        mutate("machine_id", json!(7)),
        mutate("event_type", Value::Null),
        mutate("from_states", json!("m.a")),
        mutate("actions", json!([1])),
        mutate("event_payload", json!([])),
        mutate("disposition", json!("exploded")),
        mutate("origin", json!("sideways")),
        mutate("engine", json!("rust-sync")),
        mutate("actor", json!(5)),
    ] {
        assert!(matches!(TransitionRecord::from_value(&bad), Err(StoreError::Corrupt(_))), "{bad}");
    }
    for text in ["", "{", "not json"] {
        assert!(matches!(TransitionRecord::from_json(text), Err(StoreError::Corrupt(_))), "{text}");
    }
}

// ----------------------------------------------------------------------------------- stores

fn stores(dir: &tempfile::TempDir) -> Vec<(&'static str, Box<dyn TransitionLogStore>)> {
    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
    let mut all: Vec<(&'static str, Box<dyn TransitionLogStore>)> = vec![
        ("memory", Box::new(MemoryLog::new())),
        ("jsonl", Box::new(JsonLinesLog::new(dir.path().join("log").join("t.jsonl")).unwrap())),
    ];
    #[cfg(feature = "sqlite")]
    all.push(("sqlite", Box::new(SqliteLog::open(dir.path().join("t.db")).unwrap())));
    all
}

#[test]
fn append_next_assigns_gap_free_sequences_per_key() {
    let dir = tempfile::tempdir().unwrap();
    for (name, log) in stores(&dir) {
        assert_eq!(log.next_seq("a").unwrap(), 1, "{name}");
        for expected in 1..=3 {
            let stored = log.append_next(record("a", 0, 10.0)).unwrap();
            assert_eq!(stored.seq, expected, "{name}");
        }
        assert_eq!(log.append_next(record("b", 0, 10.0)).unwrap().seq, 1, "{name}");
        assert_eq!(log.next_seq("a").unwrap(), 4, "{name}");
        assert_eq!(log.next_seq("b").unwrap(), 2, "{name}");
    }
}

#[test]
fn read_filters_by_key_after_seq_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    for (name, log) in stores(&dir) {
        for seq in 1..=5 {
            log.append(&record("a", seq, 10.0)).unwrap();
        }
        log.append(&record("b", 1, 10.0)).unwrap();
        let all = log.read("a", 0, 1000).unwrap();
        assert_eq!(all.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5], "{name}");
        let page = log.read("a", 2, 2).unwrap();
        assert_eq!(page.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4], "{name}");
        assert_eq!(log.read("a", 5, 10).unwrap().len(), 0, "{name}");
        assert_eq!(log.read("a", 0, 0).unwrap().len(), 0, "{name}: limit 0");
        assert_eq!(log.read("nobody", 0, 10).unwrap().len(), 0, "{name}");
        assert_eq!(all[0], record("a", 1, 10.0), "{name}: records round-trip");
    }
}

#[test]
fn append_rejects_records_no_store_may_persist() {
    let dir = tempfile::tempdir().unwrap();
    for (name, log) in stores(&dir) {
        assert!(log.append(&record("a", 0, 1.0)).is_err(), "{name}: seq 0");
        assert!(log.append(&record("a", 1, f64::NAN)).is_err(), "{name}: NaN ts");
        assert!(log.append(&record("a", 1, f64::INFINITY)).is_err(), "{name}: infinite ts");
        assert_eq!(log.next_seq("a").unwrap(), 1, "{name}: nothing was written");
    }
}

#[test]
fn purge_older_than_and_forget_report_counts() {
    let dir = tempfile::tempdir().unwrap();
    for (name, log) in stores(&dir) {
        log.append(&record("a", 1, 100.0)).unwrap();
        log.append(&record("a", 2, 200.0)).unwrap();
        log.append(&record("b", 1, 50.0)).unwrap();
        assert!(log.purge_older_than(f64::NAN).is_err(), "{name}: NaN would erase everything");
        assert!(log.purge_older_than(f64::NEG_INFINITY).is_err(), "{name}");
        assert_eq!(log.purge_older_than(150.0).unwrap(), 2, "{name}");
        assert_eq!(log.read("a", 0, 10).unwrap().len(), 1, "{name}");
        assert_eq!(log.read("b", 0, 10).unwrap().len(), 0, "{name}");
        assert_eq!(log.forget("a").unwrap(), 1, "{name}");
        assert_eq!(log.forget("a").unwrap(), 0, "{name}");
        assert_eq!(log.next_seq("a").unwrap(), 1, "{name}");
    }
}

#[test]
fn jsonl_writes_one_sorted_key_line_per_record_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit").join("t.jsonl");
    {
        let log = JsonLinesLog::new(&path).unwrap();
        assert_eq!(log.path(), path);
        log.append(&golden_transition()).unwrap();
        log.append_next(record("order:1", 0, 7.0)).unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], GOLDEN_TRANSITION, "byte-identical to the Python writer");
    assert!(text.ends_with('\n'));

    let reopened = JsonLinesLog::new(&path).unwrap();
    assert_eq!(reopened.next_seq("order:1").unwrap(), 5, "seq continues after the max on disk");
    assert_eq!(reopened.read("order:1", 0, 10).unwrap().len(), 2);
}

#[test]
fn jsonl_reads_python_files_with_bom_and_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("py.jsonl");
    std::fs::write(&path, format!("\u{feff}{GOLDEN_TRANSITION}\r\n\r\n{GOLDEN_ENGINE}\r\n"))
        .unwrap();
    let log = JsonLinesLog::new(&path).unwrap();
    assert_eq!(log.read("order:1", 0, 10).unwrap(), [golden_transition()]);
    assert_eq!(log.read("x", 0, 10).unwrap().len(), 1);
}

#[test]
fn jsonl_names_the_corrupt_line_and_never_skips_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.jsonl");
    let torn = &GOLDEN_TRANSITION[..40];
    std::fs::write(&path, format!("{GOLDEN_TRANSITION}\n{torn}")).unwrap();
    let log = JsonLinesLog::new(&path).unwrap();
    let Err(StoreError::Corrupt(message)) = log.read("order:1", 0, 10) else {
        panic!("a torn last line is corruption");
    };
    assert!(message.contains(":2:") && message.contains("truncate"), "{message}");
    assert!(matches!(log.next_seq("order:1"), Err(StoreError::Corrupt(_))));

    std::fs::write(&path, "{\"machine_id\": \"m\"}\n").unwrap();
    assert!(matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(m)) if m.contains(":1:")));
    std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    assert!(matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(m)) if m.contains("UTF-8")));
}

#[test]
fn jsonl_purge_rewrites_atomically_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.jsonl");
    let log = JsonLinesLog::new(&path).unwrap();
    log.append(&record("a", 1, 1.0)).unwrap();
    log.append(&record("a", 2, 9.0)).unwrap();
    assert_eq!(log.purge_older_than(5.0).unwrap(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    assert!(!dir.path().join("p.jsonl.tmp").exists());
    assert_eq!(log.next_seq("a").unwrap(), 3, "the seq cache is refreshed after a rewrite");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_log_uses_the_python_table_and_rejects_foreign_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let log = SqliteLog::open(&path).unwrap();
        log.append(&golden_transition()).unwrap();
        assert!(log.append(&golden_transition()).is_err(), "duplicate (key, seq)");
        assert_eq!(log.append_next(record("order:1", 0, 1.0)).unwrap().seq, 4);
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    let (machine_id, seq, ts, text): (String, i64, f64, String) = conn
        .query_row("SELECT machine_id, seq, ts, record FROM transitions WHERE seq = 3", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    assert_eq!((machine_id.as_str(), seq), ("order:1", 3));
    assert!((ts - 1_700_000_000.5).abs() < 1e-6);
    assert!(text.starts_with(r#"{"actions": ["charge", "notify"]"#));
    drop(conn);

    let foreign = dir.path().join("foreign.db");
    rusqlite::Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE transitions (id INTEGER, note TEXT)")
        .unwrap();
    assert!(matches!(SqliteLog::open(&foreign), Err(StoreError::Other(m)) if m.contains("schema")));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_read_detects_tampered_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let log = SqliteLog::open(&path).unwrap();
    log.append_next(record("a", 0, 1.0)).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE transitions SET record = 'garbage'", []).unwrap();
    assert!(matches!(log.read("a", 0, 10), Err(StoreError::Corrupt(_))));
    conn.execute("UPDATE transitions SET record = ?1", [record("a", 9, 1.0).to_json()]).unwrap();
    assert!(
        matches!(log.read("a", 0, 10), Err(StoreError::Corrupt(m)) if m.contains("disagrees")),
        "the row key and its record must agree"
    );
    conn.execute("UPDATE transitions SET seq = 0", []).unwrap();
    assert!(matches!(log.read("a", 0, 10), Err(StoreError::Corrupt(m)) if m.contains("seq < 1")));
}

#[test]
fn append_next_is_atomic_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    for (name, log) in stores(&dir) {
        let log: Arc<dyn TransitionLogStore> = Arc::from(log);
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    (0..5).for_each(|_| {
                        log.append_next(record("k", 0, 1.0)).unwrap();
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let seqs: Vec<u64> = log.read("k", 0, 100).unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, (1..=30).collect::<Vec<_>>(), "{name}");
    }
}

// --------------------------------------------------------------------------------- redaction

#[test]
fn redact_masks_sensitive_keys_recursively_without_mutating() {
    let input = json!({
        "amount": 5,
        "Password": "p",
        "x-api-key": "k",
        "nested": {"apiKey_2": 1, "ok": [{"token": "t", "name": "n"}]},
        "list": [{"email": "a@b"}]
    });
    let before = input.clone();
    let out = redact(&input, DEFAULT_REDACT_KEYS);
    assert_eq!(input, before);
    assert_eq!(
        out,
        json!({
            "amount": 5,
            "Password": "***",
            "x-api-key": "***",
            "nested": {"apiKey_2": "***", "ok": [{"token": "***", "name": "n"}]},
            "list": [{"email": "***"}]
        })
    );
    assert_eq!(
        redact(&json!({"X-Custom-Token": 1}), &["x_custom_token"]),
        json!({"X-Custom-Token": "***"})
    );
    assert_eq!(redact(&json!({"password": 1}), &[] as &[&str]), json!({"password": 1}));
}

// ---------------------------------------------------------------------------------- observer

const MACHINE: &str = r#"{"id":"m","version":"1.0","initial":"a","states":{
    "a":{"after":{"100":"timed"},
         "on":{"PAY":{"target":"b","actions":["charge","notify"]}, "STAY":{"actions":"noop"}}},
    "timed":{"on":{"BACK":"a"}},
    "b":{"on":{"FIN":"c"}},
    "c":{"type":"final"}}}"#;

fn logic() -> MachineLogic<Value> {
    MachineLogic::dynamic()
        .action("charge", |_| Ok(()))
        .action("notify", |_| Ok(()))
        .action("noop", |_| Ok(()))
}

fn run(
    observer: TransitionLogObserver,
    machine: &str,
    drive: impl FnOnce(&mut SyncInterpreter, &SimulatedClock),
) {
    let clock = SimulatedClock::default();
    let mut interp = SyncInterpreter::with_clock(
        Machine::from_json(machine).unwrap(),
        logic(),
        Arc::new(clock.clone()),
    );
    interp.observe(Arc::new(observer));
    interp.start().unwrap();
    drive(&mut interp, &clock);
}

#[test]
fn observer_records_from_to_states_and_actions() {
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, _| {
        interp.send(xstate_core::Event::with_payload("PAY", json!({"amount": 5}))).unwrap();
        interp.send("NOPE").unwrap();
        interp.send("FIN").unwrap();
    });
    let rows = log.read("m", 0, 100).unwrap();
    assert_eq!(rows.len(), 3);

    let pay = &rows[0];
    assert_eq!((pay.seq, pay.event_type.as_str()), (1, "PAY"));
    assert_eq!(pay.from_states, ["m.a"]);
    assert_eq!(pay.to_states, ["m.b"]);
    assert_eq!(pay.actions, ["charge", "notify"]);
    assert_eq!(pay.disposition, Disposition::Transition);
    assert_eq!(pay.event_payload, object(&json!({"amount": 5})));
    assert_eq!((pay.origin, pay.engine), (Origin::External, false));
    assert_eq!(pay.machine_version, "1.0");
    assert!(pay.ts > 1.0e9, "wall clock seconds");

    let nope = &rows[1];
    assert_eq!(nope.disposition, Disposition::Unhandled);
    assert_eq!(nope.from_states, ["m.b"], "from-states are captured before the step");
    assert_eq!(nope.to_states, ["m.b"]);
    assert_eq!(nope.actions.len(), 0);

    let fin = &rows[2];
    assert_eq!(
        (fin.from_states.as_slice(), fin.to_states.as_slice()),
        (&strings(&["m.b"])[..], &strings(&["m.c"])[..])
    );
    assert_eq!(fin.seq, 3);
}

#[test]
fn observer_tracks_states_across_many_sends() {
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, _| {
        interp.send("PAY").unwrap();
        interp.send("FIN").unwrap();
    });
    let rows = log.read("m", 0, 100).unwrap();
    let chain: Vec<(&[String], &[String])> =
        rows.iter().map(|r| (r.from_states.as_slice(), r.to_states.as_slice())).collect();
    assert_eq!(
        rows[1].from_states, rows[0].to_states,
        "each step starts where the last ended: {chain:?}"
    );
}

#[test]
fn observer_can_skip_non_transitions() {
    let log = Arc::new(MemoryLog::new());
    run(
        TransitionLogObserver::new(log.clone()).include_non_transitions(false),
        MACHINE,
        |interp, _| {
            interp.send("NOPE").unwrap();
            interp.send("PAY").unwrap();
            interp.send("NOPE").unwrap();
        },
    );
    let rows = log.read("m", 0, 100).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "PAY");
    assert_eq!(rows[0].seq, 1);
}

#[test]
fn observer_redacts_payloads_and_honours_a_custom_denylist() {
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, _| {
        let payload = json!({"amount": 5, "password": "hunter2", "pan": {"card": 1}});
        interp.send(xstate_core::Event::with_payload("PAY", payload)).unwrap();
    });
    let rows = log.read("m", 0, 10).unwrap();
    assert_eq!(
        rows[0].event_payload,
        object(&json!({"amount": 5, "password": "***", "pan": "***"}))
    );

    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()).redact_keys(["amount"]), MACHINE, |interp, _| {
        let payload = json!({"amount": 5, "password": "visible"});
        interp.send(xstate_core::Event::with_payload("PAY", payload)).unwrap();
    });
    let rows = log.read("m", 0, 10).unwrap();
    assert_eq!(rows[0].event_payload, object(&json!({"amount": "***", "password": "visible"})));
}

#[test]
fn observer_logs_timer_events_as_engine_internal_in_python_spelling() {
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, clock| {
        interp.advance(clock, 100.0).unwrap();
        interp.send("BACK").unwrap();
    });
    let rows = log.read("m", 0, 10).unwrap();
    let timer = &rows[0];
    assert_eq!(timer.event_type, "after.100.m.a");
    assert!(timer.engine);
    assert_eq!(timer.origin, Origin::Internal);
    assert_eq!(timer.event_payload, object(&json!({"kind": "after"})));
    assert_eq!(
        (timer.from_states.as_slice(), timer.to_states.as_slice()),
        (&strings(&["m.a"])[..], &strings(&["m.timed"])[..])
    );
    assert_eq!(rows[1].origin, Origin::External);
}

#[test]
fn observer_logs_service_completions_like_python() {
    let machine = r#"{"id":"m","initial":"a","states":{
        "a":{"invoke":{"src":"fetch","id":"fetch","onDone":"b","onError":"e"}},
        "b":{},"e":{}}}"#;
    let log = Arc::new(MemoryLog::new());
    let clock = SimulatedClock::default();
    let logic = MachineLogic::dynamic().service("fetch", |_, _| Ok(json!({"ok": true})));
    let mut interp =
        SyncInterpreter::with_clock(Machine::from_json(machine).unwrap(), logic, Arc::new(clock));
    interp.observe(Arc::new(TransitionLogObserver::new(log.clone())));
    interp.start().unwrap();
    let rows = log.read("m", 0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "done.invoke.fetch");
    assert!(rows[0].engine);
    assert_eq!(
        rows[0].event_payload,
        object(&json!({"kind": "done", "src": "fetch", "data": {"ok": true}}))
    );
    assert_eq!(rows[0].to_states, ["m.b"]);
}

#[test]
fn observer_records_deferred_events_once() {
    let machine = r#"{"id":"m","initial":"a","onUnhandled":"defer","states":{
        "a":{"on":{"GO":"b"}},"b":{"on":{"LATE":"c"}},"c":{}}}"#;
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), machine, |interp, _| {
        interp.send("LATE").unwrap();
        interp.send("GO").unwrap();
    });
    let rows = log.read("m", 0, 10).unwrap();
    let dispositions: Vec<Disposition> = rows.iter().map(|r| r.disposition).collect();
    assert_eq!(
        dispositions,
        [Disposition::Deferred, Disposition::Transition, Disposition::Transition],
        "parked once, then GO, then the released LATE: {rows:?}"
    );
    assert_eq!(rows[2].event_type, "LATE");
    assert_eq!(rows[2].to_states, ["m.c"]);
}

#[test]
fn observer_records_events_sent_to_a_finished_machine_as_errors() {
    let log = Arc::new(MemoryLog::new());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, _| {
        interp.send("PAY").unwrap();
        interp.send("FIN").unwrap();
        interp.send("PAY").unwrap();
    });
    let rows = log.read("m", 0, 10).unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last.disposition, Disposition::Error);
    assert_eq!(last.error.as_ref().unwrap()["type"], "InterpreterStoppedError");
    assert_eq!(last.from_states, ["m.c"]);
}

#[test]
fn observer_records_step_failures() {
    let machine = r#"{"id":"m","initial":"a","states":{
        "a":{"on":{"BOOM":{"target":"b","actions":["fail"]}}},"b":{}}}"#;
    let log = Arc::new(MemoryLog::new());
    let mut interp = SyncInterpreter::new(
        Machine::from_json(machine).unwrap(),
        MachineLogic::dynamic().action("fail", |_| Err(xstate_core::Error::logic("kaboom"))),
    );
    interp.observe(Arc::new(TransitionLogObserver::new(log.clone())));
    interp.start().unwrap();
    assert!(interp.send("BOOM").is_err());
    let rows = log.read("m", 0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].disposition, Disposition::Error);
    assert_eq!(rows[0].from_states, ["m.a"]);
    assert_eq!(rows[0].to_states, ["m.a"]);
    assert_eq!(rows[0].error.as_ref().unwrap()["type"], "TransitionFailedError");
    assert!(rows[0].error.as_ref().unwrap()["message"].as_str().unwrap().contains("kaboom"));
}

#[test]
fn observer_audit_keys_and_log_key_override() {
    let log = Arc::new(MemoryLog::new());
    run(
        TransitionLogObserver::new(log.clone()).machine_id("order:42").audit(AuditKeys::default()),
        MACHINE,
        |interp, _| {
            let payload =
                json!({"actor": "alice", "reason": "looks fine", "correlation_id": "c-1"});
            interp.send(xstate_core::Event::with_payload("PAY", payload)).unwrap();
        },
    );
    assert_eq!(log.read("m", 0, 10).unwrap().len(), 0);
    let rows = log.read("order:42", 0, 10).unwrap();
    assert_eq!(rows[0].actor.as_deref(), Some("alice"));
    assert_eq!(rows[0].reason.as_deref(), Some("looks fine"));
    assert_eq!(rows[0].correlation_id.as_deref(), Some("c-1"));
}

#[test]
fn observer_output_round_trips_through_a_jsonl_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(JsonLinesLog::new(dir.path().join("o.jsonl")).unwrap());
    run(TransitionLogObserver::new(log.clone()), MACHINE, |interp, _| {
        interp.send("PAY").unwrap();
        interp.send("FIN").unwrap();
    });
    let rows = JsonLinesLog::new(dir.path().join("o.jsonl")).unwrap().read("m", 0, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].to_states, ["m.b"]);
    for row in &rows {
        assert_eq!(&TransitionRecord::from_json(&row.to_json()).unwrap(), row);
    }
}
