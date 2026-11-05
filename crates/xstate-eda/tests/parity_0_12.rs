#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Behaviour added for Python 0.12.0 parity: replay refusal of redacted dead letters, store
//! outages that are requeued without counting an attempt, and non-finite floats in envelopes.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::Machine;
use xstate_eda::{
    DeadLetter, DeadLetterSink, DeadLetterStore, Dispatch, EdaError, Envelope, EnvelopeError,
    EnvelopeHandler, HandlerError, InboundDispatcher, MemoryDeadLetterStore, ReplayOptions,
    machines_by_type, replay_dead_letter,
};
use xstate_persistence::{ReceiptJson, StoreError};

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

fn stored(data: Value) -> (Arc<MemoryDeadLetterStore>, InboundDispatcher<impl EnvelopeHandler>) {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let mut rec = DeadLetter::new("counter", "", json!({"type": "xsm.counter.ADD"}), 1.0);
    rec.id = "r".into();
    let mut env = add("s", 1).to_dict();
    env.insert("data".into(), data);
    rec.envelope = Some(Value::Object(env));
    dlq.put(rec).unwrap();
    (dlq, InboundDispatcher::new(machines(), ok))
}

#[test]
fn replay_refuses_a_dead_letter_redacted_at_capture_unless_forced() {
    for data in [json!({"n": "***"}), json!({"a": [{"b": ["***"]}]})] {
        let (dlq, disp) = stored(data);
        let err = replay_dead_letter(&*dlq, "r", &disp, &ReplayOptions::new("why")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the stored envelope data was redacted at capture ('***'); replaying it would send \
             masked values (use --force to replay anyway)"
        );
        let forced = ReplayOptions { force: true, ..ReplayOptions::new("why") };
        assert_eq!(replay_dead_letter(&*dlq, "r", &disp, &forced).unwrap().outcome, "would_replay");
    }
    // the sentinel only counts as a whole leaf: a longer string or a key is not redaction
    let (dlq, disp) = stored(json!({"***": "x", "note": "***!"}));
    let res = replay_dead_letter(&*dlq, "r", &disp, &ReplayOptions::new("why")).unwrap();
    assert_eq!(res.outcome, "would_replay");
}

fn store_down(_: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError> {
    Err(StoreError::Unavailable("db down".into()).into())
}

#[test]
fn a_state_store_outage_in_the_run_is_requeued_without_counting_an_attempt() {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), store_down)
        .max_attempts(2)
        .unwrap()
        .dead_letters(dlq.clone());
    let env = add("a", 1);
    for _ in 0..5 {
        let res = disp.handle(&env, Some("t"));
        assert_eq!((res.retried, res.dead_lettered), (1, 0), "{res:?}");
    }
    assert_eq!(dlq.count().unwrap(), 0, "never dead-lettered as poison");
    assert_eq!(disp.attempts_of(&env), 0, "no attempt was counted");
    // the other errors Python models as StoreError are outages too
    let lock =
        StoreError::LockTimeout { key: "k".into(), timeout: Duration::from_secs(1), holder: None };
    for err in [lock, StoreError::InvalidKey("k".into())] {
        assert!(matches!(HandlerError::from(err), HandlerError::Requeue { .. }));
    }
    // a corrupt snapshot is the message's problem, not an outage
    assert!(matches!(
        HandlerError::from(StoreError::Corrupt("bad".into())),
        HandlerError::Failed { .. }
    ));
}

struct DownSink;

impl DeadLetterSink for DownSink {
    fn put(&self, _: DeadLetter) -> xstate_eda::Result<()> {
        Err(EdaError::Store(StoreError::Unavailable("dlq down".into())))
    }
}

#[test]
fn a_down_dead_letter_store_does_not_double_count_the_attempt() {
    let failing = |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
        Err(HandlerError::failed("RuntimeError", "boom"))
    };
    let disp = InboundDispatcher::new(machines(), failing)
        .max_attempts(2)
        .unwrap()
        .dead_letters(Arc::new(DownSink));
    let env = add("a", 1);
    assert_eq!(disp.handle(&env, None).retried, 1);
    assert_eq!(disp.attempts_of(&env), 1);
    // the cap is hit but the dead-letter store is down: requeued, the attempt counted once
    assert_eq!(disp.handle(&env, None).retried, 1);
    assert_eq!(disp.attempts_of(&env), 2, "not counted a second time by the requeue");
}

#[test]
fn a_non_finite_float_cannot_reach_the_wire_as_nan() {
    // serde_json::Value cannot hold NaN / Infinity: they become `null` on construction, so
    // `to_json` can never emit the invalid tokens Python refuses with EnvelopeCorruptError
    let env = Envelope::new("t")
        .subject("s")
        .data(json!({"a": f64::NAN, "b": f64::INFINITY}))
        .build()
        .unwrap();
    let text = env.to_json(1 << 20).unwrap();
    assert!(!text.contains("NaN") && !text.contains("Infinity"), "{text}");
    // and the reading side refuses the tokens
    let raw = "{\"id\":\"1\",\"type\":\"t\",\"source\":\"s\",\"data\":{\"a\":NaN}}";
    assert!(matches!(Envelope::from_json(raw, 1000), Err(EnvelopeError::Corrupt(_))));
}
