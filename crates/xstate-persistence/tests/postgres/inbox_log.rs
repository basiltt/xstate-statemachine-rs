//! The inbox and the transition log on PostgreSQL: semantics, layout and contention.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::Map;
use xstate_persistence::idempotency::InboxStore;
use xstate_persistence::log::{Disposition, Origin, TransitionLogStore, TransitionRecord};
use xstate_persistence::{PostgresInbox, PostgresLog, SaveOptions, StateStore, StoreError};

use super::{Ns, ok};

// ------------------------------------------------------------------------------- the inbox

#[test]
fn inbox_claim_mark_duplicate_and_release() {
    let ns = ns!("inbox");
    let inbox = PostgresInbox::new(&ns.store());
    assert!(ok(inbox.claim("t/m/i", "k1", "fp", Some(3600.0))), "first delivery wins");
    assert!(!ok(inbox.claim("t/m/i", "k1", "other", Some(3600.0))), "a live claim holds");
    let flying = ok(inbox.get("t/m/i", "k1")).expect("in flight");
    assert_eq!((flying.fingerprint.as_str(), flying.receipt_json.clone()), ("fp", None));
    assert!(flying.expires_at.is_some_and(|t| t > 1.6e9));

    ok(inbox.mark("t/m/i", "k1", r#"{"status": 200}"#, Some(3600.0)));
    let done = ok(inbox.get("t/m/i", "k1")).expect("marked");
    assert_eq!(done.receipt_json.as_deref(), Some(r#"{"status": 200}"#));
    ok(inbox.release("t/m/i", "k1"));
    assert!(ok(inbox.get("t/m/i", "k1")).is_some(), "a marked entry is not released");

    assert!(ok(inbox.claim("t/m/i", "k2", "fp", None)));
    assert_eq!(ok(inbox.get("t/m/i", "k2")).expect("claimed").expires_at, None);
    ok(inbox.release("t/m/i", "k2"));
    assert!(ok(inbox.get("t/m/i", "k2")).is_none(), "an in-flight claim is released");
    assert!(ok(inbox.claim("t/m/i", "k2", "fp", None)), "and can be claimed again");
    assert!(ok(inbox.claim("t/m/other", "k1", "fp", None)), "scopes are independent");
}

#[test]
fn inbox_layout_is_what_python_writes() {
    let ns = ns!("inboxlayout");
    let inbox = PostgresInbox::new(&ns.store());
    ok(inbox.claim("tenant/machine/instance", "idem-1", "fp-1", Some(10.0)));
    ok(inbox.mark("tenant/machine/instance", "idem-1", r#"{"ok": true}"#, None));
    ok(inbox.claim("tenant/machine/instance", "idem-2", "fp-2", None));
    let row = |key: &str| {
        ns.raw.text(&format!(
            "SELECT concat_ws('|', scope, key, fingerprint, coalesce(receipt, '<null>'), \
             coalesce(expires_at::text, '<null>')) FROM {} WHERE key = '{key}'",
            ns.t("xsm_inbox")
        ))
    };
    assert_eq!(row("idem-1"), r#"tenant/machine/instance|idem-1|fp-1|{"ok": true}|<null>"#);
    assert_eq!(row("idem-2"), "tenant/machine/instance|idem-2|fp-2|<null>|<null>");
}

#[test]
fn inbox_ttl_expiry_purge_and_reclaim() {
    let ns = ns!("inboxttl");
    let inbox = PostgresInbox::new(&ns.store());
    assert!(ok(inbox.claim("s", "short", "fp-old", Some(0.05))));
    assert!(ok(inbox.claim("s", "long", "fp", Some(3600.0))));
    assert!(ok(inbox.claim("s", "forever", "fp", None)));
    thread::sleep(Duration::from_millis(120));
    assert!(ok(inbox.get("s", "short")).is_none(), "expired entries are invisible");
    assert!(ok(inbox.claim("s", "short", "fp-new", Some(3600.0))), "an expired claim is reclaimed");
    assert_eq!(ok(inbox.get("s", "short")).expect("reclaimed").fingerprint, "fp-new");
    assert!(ok(inbox.get("s", "short")).expect("reclaimed").receipt_json.is_none());

    assert_eq!(ok(inbox.purge_expired(Some(1.0e12))), 2, "short and long expire before 1e12");
    assert!(ok(inbox.get("s", "forever")).is_some());
    assert_eq!(ok(inbox.purge_expired(None)), 0);
    assert!(inbox.purge_expired(Some(f64::NAN)).is_err());
    assert!(inbox.claim("s", "k", "fp", Some(f64::INFINITY)).is_err());
}

#[test]
fn inbox_forget_is_exact_and_names_are_validated() {
    let ns = ns!("inboxforget");
    let inbox = PostgresInbox::new(&ns.store());
    for key in ["a", "b"] {
        ok(inbox.claim("p/m/1", key, "fp", None));
    }
    ok(inbox.claim("p/m/10", "a", "fp", None));
    ok(inbox.claim("p/m/%", "a", "fp", None));
    assert_eq!(ok(inbox.forget("p/m/1")), 2, "a prefix of another scope is not that scope");
    assert!(ok(inbox.get("p/m/10", "a")).is_some() && ok(inbox.get("p/m/%", "a")).is_some());
    assert_eq!(ok(inbox.forget("p/m/%")), 1);
    assert_eq!(ok(inbox.forget("p/m/1")), 0);
    assert!(matches!(inbox.claim("a\0b", "k", "fp", None), Err(StoreError::InvalidKey(_))));
    assert!(matches!(inbox.get("s", "k\0"), Err(StoreError::InvalidKey(_))));
}

#[test]
fn inbox_concurrent_claims_have_exactly_one_winner() {
    let ns = ns!("inboxrace");
    let inbox = Arc::new(PostgresInbox::new(&ns.store()));
    let claims: Vec<_> = (0..8)
        .map(|n| {
            let inbox = Arc::clone(&inbox);
            thread::spawn(move || ok(inbox.claim("s", "k", &format!("fp-{n}"), Some(60.0))))
        })
        .collect();
    let winners = claims
        .into_iter()
        .map(|t| t.join().unwrap_or_else(|_| panic!("panicked")))
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
}

// --------------------------------------------------------------------------------- the log

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

pub(super) fn record(machine_id: &str, seq: u64, ts: f64) -> TransitionRecord {
    TransitionRecord {
        machine_id: machine_id.into(),
        seq,
        ts,
        event_type: "GO".into(),
        event_payload: Map::new(),
        from_states: strings(&["m.a"]),
        to_states: strings(&["m.b"]),
        actions: strings(&["act"]),
        disposition: Disposition::Transition,
        actor: Some("alice".into()),
        reason: None,
        correlation_id: None,
        machine_version: "1.0".into(),
        engine: false,
        error: None,
        origin: Origin::External,
    }
}

const NONE: [u64; 0] = [];

fn seqs(records: &[TransitionRecord]) -> Vec<u64> {
    records.iter().map(|r| r.seq).collect()
}

#[test]
fn log_append_read_and_next_seq() {
    let ns = ns!("log");
    let log = PostgresLog::new(&ns.store());
    assert_eq!(ok(log.next_seq("m")), 1);
    for seq in 1..=5 {
        ok(log.append(&record("m", seq, f64::from(u32::try_from(seq).unwrap_or(0)))));
    }
    assert_eq!(ok(log.next_seq("m")), 6);
    let all = ok(log.read("m", 0, 100));
    assert_eq!(seqs(&all), [1, 2, 3, 4, 5]);
    assert_eq!(all[0], record("m", 1, 1.0));
    assert_eq!(seqs(&ok(log.read("m", 2, 2))), [3, 4]);
    assert_eq!(seqs(&ok(log.read("m", 5, 10))), NONE);
    assert_eq!(seqs(&ok(log.read("m", 0, 0))), NONE);
    assert_eq!(seqs(&ok(log.read("m", u64::MAX, 10))), NONE);
    assert_eq!(seqs(&ok(log.read("other", 0, 10))), NONE);
    assert!(log.append(&record("m", 5, 1.0)).is_err(), "a duplicate (machine_id, seq) is refused");
    assert!(log.append(&record("m", 0, 1.0)).is_err());
    assert!(log.append(&record("m", 9, f64::NAN)).is_err());
    assert_eq!(ok(log.read("m", 0, 100)).len(), 5);
}

#[test]
fn log_layout_is_what_python_writes() {
    let ns = ns!("loglayout");
    let log = PostgresLog::new(&ns.store());
    ok(log.append(&record("m:1", 7, 12.5)));
    let row = ns.raw.text(&format!(
        "SELECT concat_ws('|', machine_id, seq, ts, record) FROM {}",
        ns.t("xsm_transitions")
    ));
    assert_eq!(row, format!("m:1|7|12.5|{}", record("m:1", 7, 12.5).to_json()));
    assert!(row.contains(r#""seq": 7"#), "json.dumps spacing: {row}");
}

#[test]
fn log_append_next_never_mints_a_seq_twice_under_contention() {
    let ns = ns!("lograce");
    let log = Arc::new(PostgresLog::new(&ns.store()));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let log = Arc::clone(&log);
            thread::spawn(move || {
                (0..10).map(|_| ok(log.append_next(record("m", 0, 1.0))).seq).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap_or_else(|_| panic!("worker panicked")))
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=80).collect::<Vec<u64>>());
    assert_eq!(ok(log.read("m", 0, 1000)).len(), 80);
    assert_eq!(ok(log.next_seq("m")), 81);
}

#[test]
fn log_append_next_survives_a_writer_that_does_not_take_the_advisory_lock() {
    let ns = ns!("logmixed");
    let log = Arc::new(PostgresLog::new(&ns.store()));
    // A Python-style writer: `next_seq` then `append`, so it can lose the race, which is fine.
    let naive = {
        let log = Arc::clone(&log);
        thread::spawn(move || {
            for _ in 0..30 {
                let seq = ok(log.next_seq("m"));
                let _ = log.append(&record("m", seq, 1.0));
            }
        })
    };
    let careful: Vec<_> = (0..30).map(|_| ok(log.append_next(record("m", 0, 1.0))).seq).collect();
    naive.join().unwrap_or_else(|_| panic!("naive writer panicked"));
    let mut sorted = careful.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), careful.len(), "append_next never repeated a seq");
    let stored = seqs(&ok(log.read("m", 0, 1000)));
    assert_eq!(
        stored,
        (1..=u64::try_from(stored.len()).unwrap_or(0)).collect::<Vec<_>>(),
        "gap-free"
    );
}

#[test]
fn log_purge_and_forget() {
    let ns = ns!("logretention");
    let log = PostgresLog::new(&ns.store());
    for seq in 1..=4 {
        ok(log.append(&record("a", seq, f64::from(u32::try_from(seq).unwrap_or(0)) * 10.0)));
    }
    ok(log.append(&record("b", 1, 5.0)));
    assert_eq!(ok(log.purge_older_than(25.0)), 3, "a:10, a:20 and b:5");
    assert_eq!(seqs(&ok(log.read("a", 0, 10))), [3, 4]);
    assert_eq!(seqs(&ok(log.read("b", 0, 10))), NONE);
    assert_eq!(ok(log.next_seq("b")), 1, "an emptied log restarts at 1");
    assert!(log.purge_older_than(f64::NAN).is_err());
    assert_eq!(ok(log.forget("a")), 2);
    assert_eq!(ok(log.forget("a")), 0);
}

#[test]
fn log_corruption_is_reported() {
    let ns = ns!("logcorrupt");
    let log = PostgresLog::new(&ns.store());
    let table = ns.t("xsm_transitions");
    let insert = |machine: &str, seq: i64, json: &str| {
        ns.raw.exec(&format!("INSERT INTO {table} VALUES ('{machine}', {seq}, 1.0, '{json}')"));
    };
    insert("m", 1, &record("m", 1, 1.0).to_json());
    insert("m", 2, &record("m", 99, 1.0).to_json());
    assert!(
        matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(_))),
        "row key disagrees with its record"
    );
    ns.raw.exec(&format!("DELETE FROM {table}"));
    insert("m", 1, "not json");
    assert!(matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(_))));
    ns.raw.exec(&format!("DELETE FROM {table}"));
    insert("m", 0, &record("m", 1, 1.0).to_json());
    assert!(matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(_))), "a seq < 1 is a hole");
}

#[test]
fn forgetting_a_store_key_erases_its_log_through_the_shared_table() {
    let ns = ns!("logforget");
    let store = ns.store();
    let log = PostgresLog::new(&store);
    ok(store.save("k", "{}", &SaveOptions::default()));
    ok(log.append_next(record("k", 0, 1.0)));
    assert_eq!(
        ok(store.forget("k")).other,
        [("locks".to_owned(), 0), ("log_entries".to_owned(), 1)]
    );
    assert_eq!(seqs(&ok(log.read("k", 0, 10))), NONE);
}
