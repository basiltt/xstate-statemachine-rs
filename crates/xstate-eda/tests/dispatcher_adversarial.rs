#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Adversarial dispatcher coverage: hook panics, batch limits, key/event overrides, dedup-key
//! opt-out, inbox scope separation and the audited replay path.

mod common;

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use xstate_core::Machine;
use xstate_eda::{
    DeadLetter, DeadLetterSink, DeadLetterStore, Dispatch, EdaError, Envelope, HandlerError,
    InboundDispatcher, MemoryDeadLetterStore, OnUnknown, SyncFakeBrokerAdapter, machines_by_type,
    sink_fn,
};
use xstate_persistence::{MemoryInbox, ReceiptJson};

const ZERO: std::time::Duration = std::time::Duration::ZERO;

fn machines() -> impl Fn(&str) -> Option<Machine> + Send + Sync + 'static {
    machines_by_type(HashMap::from([("xsm.counter.ADD".to_owned(), common::counter())]))
}

fn add(subject: &str, n: i64) -> Envelope {
    Envelope::new("xsm.counter.ADD").subject(subject).data(json!({"n": n})).build().unwrap()
}

#[allow(clippy::unnecessary_wraps)] // the shape `EnvelopeHandler` requires
fn ok(_: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError> {
    Ok(ReceiptJson::default())
}

#[test]
fn a_panicking_dead_letter_sink_does_not_make_handle_panic() {
    // `handle` documents "never fails and never panics on a panicking handler or hook"; a
    // panicking `DeadLetterSink::put` must therefore stay contained, whatever the envelope.
    // `max_attempts(1)` makes the counted failure dead-letter at once, which re-invokes `put`
    // outside the `catch_unwind`: the second panic is the one that can escape.
    let disp = InboundDispatcher::new(machines(), ok)
        .max_attempts(1)
        .unwrap()
        .dead_letters(Arc::new(sink_fn(|_: DeadLetter| panic!("sink bug"))));
    let corrupt = Envelope::new("xsm.counter.ADD").data(json!({"n": 1})).build().unwrap();
    let outcome = catch_unwind(AssertUnwindSafe(|| disp.handle(&corrupt, Some("t"))));
    assert!(outcome.is_ok(), "handle panicked: {outcome:?}");
}

#[test]
fn a_panicking_lookup_hook_is_contained_and_counts_towards_dead_lettering() {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(|_: &str| -> Option<Machine> { panic!("lookup bug") }, ok)
        .max_attempts(2)
        .unwrap()
        .dead_letters(dlq.clone());
    let env = add("s", 1);
    // the panic is contained: `handle` returns normally and counts a retry
    let first = catch_unwind(AssertUnwindSafe(|| disp.handle(&env, None)));
    assert!(first.is_ok(), "the lookup hook panic must not escape handle");
    assert_eq!(first.unwrap().retried, 1);
    // at the cap the panicking hook's envelope is dead-lettered
    let second = catch_unwind(AssertUnwindSafe(|| disp.handle(&env, None)));
    assert_eq!(second.unwrap().dead_lettered, 1);
    assert_eq!(dlq.get(&env.id).unwrap().unwrap().errors[0].ty, "PanicError");
}

#[test]
fn max_batch_caps_how_many_deliveries_one_run_pulls() {
    let disp = InboundDispatcher::new(machines(), ok).max_batch(2).unwrap();
    let broker = SyncFakeBrokerAdapter::new();
    for n in 0..5 {
        broker.deliver("t", &add("a", n)).unwrap();
    }
    let res = disp.run_once_sync(&broker, "t", ZERO).unwrap();
    assert_eq!(res.processed, 2, "{res:?}");
    assert_eq!(broker.pending("t"), 3);
    assert_eq!(disp.run_once_sync(&broker, "t", ZERO).unwrap().processed, 2);
    assert_eq!(disp.run_once_sync(&broker, "t", ZERO).unwrap().processed, 1);
    assert_eq!(broker.pending("t"), 0);
}

#[test]
fn key_for_prefixes_the_instance_key_and_event_type_overrides_the_name() {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let handler = move |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
        seen2.lock().unwrap().push((d.key.to_owned(), d.event.ty.to_string()));
        Ok(ReceiptJson::default())
    };
    let disp = InboundDispatcher::new(machines(), handler)
        .key_for(|env, machine| {
            format!("{}/{}", machine.id(), env.subject.as_deref().unwrap_or("none"))
        })
        .event_type(|_| "CUSTOM".to_owned());
    let res = disp.handle(&add("o-7", 3), None);
    assert_eq!(res.processed, 1, "{res:?}");
    let got = seen.lock().unwrap();
    assert_eq!(*got, [("counter/o-7".to_owned(), "CUSTOM".to_owned())]);
}

#[test]
fn a_none_dedup_key_disables_the_inbox_entirely() {
    let disp = InboundDispatcher::new(machines(), ok)
        .inbox(Arc::new(MemoryInbox::new()))
        .dedup_key(|_| None);
    let env = add("o", 5);
    assert_eq!(disp.handle(&env, None).processed, 1);
    assert_eq!(disp.handle(&env, None).processed, 1, "no dedup: the same envelope runs twice");
}

#[test]
fn the_inbox_claim_scope_separates_two_sources_sharing_an_id() {
    let disp = InboundDispatcher::new(machines(), ok).inbox(Arc::new(MemoryInbox::new()));
    let env = add("o", 6);
    assert_eq!(disp.handle(&env, None).processed, 1);
    // the same id with a different source is a different scope, so it is not a duplicate
    let mut other = env.clone();
    other.source = "xsm/other".to_owned();
    assert_eq!(disp.handle(&other, None).processed, 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn replay_writes_an_audit_entry_and_a_dry_run_does_not() {
    use xstate_eda::{ReplayOptions, SqliteDeadLetterStore, replay_dead_letter};

    let dir = tempfile::tempdir().unwrap();
    let dlq = SqliteDeadLetterStore::open(dir.path().join("dlq.sqlite")).unwrap();
    let mut rec = DeadLetter::new("counter", "", json!({"type": "xsm.counter.ADD"}), 1.0);
    rec.id = "r".into();
    rec.machine_hash = Some(common::counter().structure_hash().to_owned());
    rec.envelope = Some(Value::Object(add("s", 1).to_dict()));
    dlq.put(rec).unwrap();

    let disp = InboundDispatcher::new(machines(), ok);
    // a dry run reports but neither resolves nor audits
    let dry = replay_dead_letter(&dlq, "r", &disp, &ReplayOptions::new("why")).unwrap();
    assert_eq!(dry.outcome, "would_replay");
    assert!(dlq.get("r").unwrap().unwrap().resolved_at.is_none());
    assert_eq!(dlq.audit_log().unwrap().len(), 0);

    let real = ReplayOptions { dry_run: false, ..ReplayOptions::new("fixed it") };
    let res = replay_dead_letter(&dlq, "r", &disp, &real).unwrap();
    assert_eq!(res.outcome, "processed");
    assert!(dlq.get("r").unwrap().unwrap().resolved_at.is_some());
    let log = dlq.audit_log().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].action.as_str(), log[0].record_id.as_deref()), ("replay", Some("r")));
    assert_eq!(log[0].reason, "fixed it");
    assert_eq!(log[0].detail, json!({"outcome": "processed", "forced": false}));
}

#[test]
fn on_unknown_ignore_acks_without_a_handler() {
    let disp = InboundDispatcher::new(machines(), ok).on_unknown(OnUnknown::Ignore);
    let unknown = Envelope::new("xsm.other.GO").subject("o").build().unwrap();
    let res = disp.handle(&unknown, None);
    assert_eq!((res.ignored, res.outcomes[0].1.as_str()), (1, "ignored"));
}

#[test]
fn max_in_flight_of_zero_is_refused() {
    assert!(matches!(
        InboundDispatcher::new(machines(), ok).max_in_flight(0),
        Err(EdaError::InvalidArgument(_))
    ));
}

#[test]
fn a_handler_returning_instance_done_is_dead_lettered_at_once_without_retries() {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(
        machines(),
        |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            Err(HandlerError::InstanceDone { message: "already finished".into() })
        },
    )
    .max_attempts(10)
    .unwrap()
    .dead_letters(dlq.clone());
    let env = add("s", 1);
    let res = disp.handle(&env, None);
    assert_eq!(res.outcomes[0].1, "dead_lettered:instance_done");
    assert_eq!(dlq.get(&env.id).unwrap().unwrap().attempts, Some(1));
    // the second attempt counter was never bumped: instance_done never retries
    assert_eq!(disp.attempts_of(&env), 0);
}

// -- trait-object sink usable as declared ------------------------------------------------------

#[test]
fn the_sink_fn_and_store_are_object_safe() {
    let sink: Arc<dyn DeadLetterSink> = Arc::new(sink_fn(|_| Ok(())));
    sink.put(DeadLetter::new("m", "", json!({}), 0.0)).unwrap();
    let _store: Arc<dyn DeadLetterStore> = Arc::new(MemoryDeadLetterStore::new());
}
