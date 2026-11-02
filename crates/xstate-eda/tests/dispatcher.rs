#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Behaviour of `InboundDispatcher`: per-subject order, retries then dead letters (X0.8),
//! inbox dedup, unknown / corrupt envelopes, panics, the async consumer and replay.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic};
use xstate_eda::{
    BrokerAdapter, BrokerError, DeadLetterSink, DeadLetterStore, Delivery, Dispatch, Envelope,
    EnvelopeHandler, FakeBrokerAdapter, HandlerError, InboundDispatcher, MemoryDeadLetterStore,
    OnUnknown, ReplayOptions, SyncBrokerAdapter, SyncFakeBrokerAdapter, machines_by_type,
    replay_dead_letter, sink_fn,
};
use xstate_persistence::receipts::ReceiptFlags;
use xstate_persistence::{
    MemoryInbox, MemoryStore, PersistedOptions, ReceiptJson, StateStore, persisted,
};

const ZERO: Duration = Duration::ZERO;

/// Fails `ADD` for every `n` in the set (each failure consumes one entry when `once`).
#[derive(Default)]
struct Failures {
    always: Mutex<HashSet<i64>>,
    once: Mutex<HashSet<i64>>,
}

impl Failures {
    fn check(&self, n: i64) -> xstate_core::Result<()> {
        if self.always.lock().unwrap().contains(&n) || self.once.lock().unwrap().remove(&n) {
            return Err(xstate_core::Error::logic(format!("boom {n}")));
        }
        Ok(())
    }
}

struct Harness {
    store: Arc<MemoryStore>,
    failures: Arc<Failures>,
}

impl Harness {
    fn new() -> Self {
        Self { store: Arc::new(MemoryStore::new()), failures: Arc::new(Failures::default()) }
    }

    fn logic(&self) -> MachineLogic<Value> {
        let failures = Arc::clone(&self.failures);
        MachineLogic::dynamic().action("record", move |scope| {
            let n = scope.event.payload["n"].as_i64().unwrap_or(-1);
            failures.check(n)?;
            scope.context["seen"].as_array_mut().unwrap().push(json!(n));
            Ok(())
        })
    }

    fn handler(&self) -> impl EnvelopeHandler + use<> {
        let store = Arc::clone(&self.store);
        let logic = self.logic();
        move |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            let options = PersistedOptions::default();
            let receipt = persisted(&*store, d.key, d.machine, &logic, &options, |interp, _| {
                let r = interp.send(d.event.clone())?;
                Ok(ReceiptJson::from_receipt(&r, ReceiptFlags::default(), None))
            })?;
            HandlerError::check_receipt(receipt)
        }
    }

    fn seen(&self, key: &str) -> Vec<i64> {
        let rec = self.store.load(key).unwrap().unwrap();
        let snap: Value = serde_json::from_str(&rec.snapshot).unwrap();
        snap["context"]["seen"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect()
    }
}

fn machines() -> impl Fn(&str) -> Option<Machine> + Send + Sync + 'static {
    machines_by_type(HashMap::from([("xsm.counter.ADD".to_owned(), common::counter())]))
}

fn add(subject: &str, n: i64) -> Envelope {
    Envelope::new("xsm.counter.ADD").subject(subject).data(json!({"n": n})).build().unwrap()
}

#[test]
fn many_envelopes_keep_per_subject_order() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = SyncFakeBrokerAdapter::new();
    for n in 0..300 {
        broker.deliver("orders", &add(&format!("s-{}", n % 7), n)).unwrap();
    }
    let res = disp.run_once_sync(&broker, "orders", ZERO).unwrap();
    assert_eq!(res.processed, 300);
    assert_eq!(broker.acked().len(), 300);
    assert_eq!(broker.in_flight(), 0);
    for s in 0..7 {
        let expected: Vec<i64> = (0..300).filter(|n| n % 7 == s).collect();
        assert_eq!(h.seen(&format!("s-{s}")), expected);
    }
}

#[test]
fn a_failure_requeues_the_rest_of_its_subject_in_order() {
    let h = Harness::new();
    h.failures.once.lock().unwrap().insert(1);
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = SyncFakeBrokerAdapter::new();
    for n in 0..4 {
        broker.deliver("t", &add("a", n)).unwrap();
    }
    broker.deliver("t", &add("b", 9)).unwrap();
    let first = disp.run_once_sync(&broker, "t", ZERO).unwrap();
    assert_eq!((first.processed, first.retried), (2, 1), "{first:?}");
    assert_eq!(broker.pending("t"), 3);
    let second = disp.run_once_sync(&broker, "t", ZERO).unwrap();
    assert_eq!(second.processed, 3);
    assert_eq!(h.seen("a"), [0, 1, 2, 3]);
    assert_eq!(h.seen("b"), [9]);
}

#[test]
fn poison_is_dead_lettered_and_acked_after_max_attempts() {
    let h = Harness::new();
    h.failures.always.lock().unwrap().insert(7);
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), h.handler())
        .max_attempts(3)
        .unwrap()
        .dead_letters(dlq.clone());
    let broker = SyncFakeBrokerAdapter::new();
    let env = add("p", 7);
    broker.deliver("t", &env).unwrap();
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let res = disp.run_once_sync(&broker, "t", ZERO).unwrap();
        outcomes.extend(res.outcomes.into_iter().map(|(_, o)| o));
    }
    assert_eq!(outcomes, ["retry", "retry", "dead_lettered:max_attempts"]);
    assert_eq!(broker.pending("t"), 0);
    let rec = dlq.get(&env.id).unwrap().unwrap();
    assert_eq!(rec.reason, "max_attempts");
    assert_eq!(rec.attempts, Some(3));
    assert_eq!(rec.topic.as_deref(), Some("t"));
    assert_eq!(rec.errors[0].source, "dispatcher");
    assert_eq!(rec.errors[0].ty, "TransitionFailedError");
    assert!(rec.errors[0].message.contains("boom 7"), "{:?}", rec.errors);
    assert_eq!(rec.machine_hash.as_deref(), Some(common::counter().structure_hash()));
}

#[test]
fn the_attempt_extension_counts_towards_the_cap() {
    let h = Harness::new();
    h.failures.always.lock().unwrap().insert(1);
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), h.handler()).dead_letters(dlq.clone());
    let env = add("x", 1).with_attempt(4);
    let res = disp.handle(&env, None);
    assert_eq!(res.dead_lettered, 1);
    assert_eq!(dlq.get(&env.id).unwrap().unwrap().attempts, Some(5));
}

#[test]
fn unknown_and_corrupt_envelopes_are_dead_lettered_never_raised() {
    let h = Harness::new();
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), h.handler()).dead_letters(dlq.clone());
    let broker = SyncFakeBrokerAdapter::new();
    let unknown = Envelope::new("xsm.other.GO").subject("o").build().unwrap();
    let no_subject = Envelope::new("xsm.counter.ADD").data(json!({"n": 1})).build().unwrap();
    let list_data = Envelope::new("xsm.counter.ADD").subject("o").data(json!([1])).build().unwrap();
    for env in [&unknown, &no_subject, &list_data] {
        broker.deliver("t", env).unwrap();
    }
    let res = disp.run_once_sync(&broker, "t", ZERO).unwrap();
    assert_eq!(res.dead_lettered, 3);
    assert_eq!(broker.acked().len(), 3);
    assert_eq!(dlq.get(&unknown.id).unwrap().unwrap().reason, "unknown_event");
    let corrupt = dlq.get(&no_subject.id).unwrap().unwrap();
    assert_eq!(corrupt.reason, "corrupt");
    assert_eq!(corrupt.errors[0].message, "envelope has no subject (the instance key)");
    assert_eq!(dlq.get(&list_data.id).unwrap().unwrap().errors[0].ty, "EnvelopeCorruptError");

    let ignoring = InboundDispatcher::new(machines(), h.handler()).on_unknown(OnUnknown::Ignore);
    let res = ignoring.handle(&unknown, None);
    assert_eq!((res.ignored, res.outcomes[0].1.as_str()), (1, "ignored"));
}

#[test]
fn inbox_answers_redeliveries_and_refuses_reused_ids() {
    let h = Harness::new();
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), h.handler())
        .inbox(Arc::new(MemoryInbox::new()))
        .dead_letters(dlq.clone());
    let env = add("i", 5);
    assert_eq!(disp.handle(&env, None).processed, 1);
    assert_eq!(disp.handle(&env, None).duplicates, 1);
    assert_eq!(h.seen("i"), [5]);
    let mut forged = env.clone();
    forged.data = json!({"n": 6});
    let res = disp.handle(&forged, None);
    assert_eq!(res.outcomes[0].1, "dead_lettered:idempotency_mismatch");
    assert_eq!(dlq.get(&env.id).unwrap().unwrap().errors[0].ty, "IdempotencyMismatchError");
    assert_eq!(h.seen("i"), [5]);
}

#[test]
fn a_failed_delivery_releases_its_inbox_claim() {
    let h = Harness::new();
    h.failures.once.lock().unwrap().insert(3);
    let disp = InboundDispatcher::new(machines(), h.handler()).inbox(Arc::new(MemoryInbox::new()));
    let env = add("r", 3);
    assert_eq!(disp.handle(&env, None).retried, 1);
    assert_eq!(disp.handle(&env, None).processed, 1);
    assert_eq!(h.seen("r"), [3]);
}

#[test]
fn a_finished_instance_is_dead_lettered_at_once() {
    let machine = Machine::from_value(&json!({
        "id": "job", "initial": "open",
        "states": {"open": {"on": {"CLOSE": "closed"}}, "closed": {"type": "final"}}
    }))
    .unwrap();
    let store = Arc::new(MemoryStore::new());
    let handler = {
        let store = Arc::clone(&store);
        move |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            let receipt = persisted(
                &*store,
                d.key,
                d.machine,
                &MachineLogic::dynamic(),
                &PersistedOptions::default(),
                |i, _| {
                    let r = i.send(d.event.clone())?;
                    Ok(ReceiptJson::from_receipt(&r, ReceiptFlags::default(), None))
                },
            )?;
            HandlerError::check_receipt(receipt)
        }
    };
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(
        machines_by_type(HashMap::from([("xsm.job.CLOSE".into(), machine)])),
        handler,
    )
    .dead_letters(dlq.clone());
    let close = || Envelope::new("xsm.job.CLOSE").subject("j").build().unwrap();
    assert_eq!(disp.handle(&close(), None).processed, 1);
    let late = close();
    assert_eq!(disp.handle(&late, None).outcomes[0].1, "dead_lettered:instance_done");
    assert_eq!(dlq.get(&late.id).unwrap().unwrap().errors[0].ty, "InterpreterStoppedError");
}

#[test]
fn a_panicking_handler_is_retried_then_dead_lettered() {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(
        machines(),
        |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> { panic!("handler bug") },
    )
    .max_attempts(2)
    .unwrap()
    .dead_letters(dlq.clone());
    let env = add("z", 1);
    assert_eq!(disp.handle(&env, None).retried, 1);
    assert_eq!(disp.handle(&env, None).dead_lettered, 1);
    let rec = dlq.get(&env.id).unwrap().unwrap();
    assert_eq!(
        (rec.errors[0].ty.as_str(), rec.errors[0].message.as_str()),
        ("PanicError", "handler bug")
    );
}

#[test]
fn a_dead_letter_store_outage_requeues_without_counting() {
    let disp = InboundDispatcher::new(
        machines(),
        |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            Err(HandlerError::failed("RuntimeError", "nope"))
        },
    )
    .max_attempts(1)
    .unwrap()
    .dead_letters(Arc::new(sink_fn(|_| {
        Err(xstate_eda::EdaError::InvalidArgument("dlq down".into()))
    })));
    let env = add("d", 1);
    for _ in 0..3 {
        assert_eq!(disp.handle(&env, None).retried, 1);
    }
}

#[test]
fn async_run_once_processes_subjects_concurrently() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler()).max_in_flight(4).unwrap();
    let broker = FakeBrokerAdapter::new();
    for n in 0..60 {
        broker.deliver("t", &add(&format!("s{}", n % 5), n)).unwrap();
    }
    let res = common::block_on(disp.run_once(&broker, "t", Duration::from_millis(10))).unwrap();
    assert_eq!(res.processed, 60);
    assert_eq!(broker.acked().len(), 60);
    for s in 0..5 {
        assert_eq!(h.seen(&format!("s{s}")), (0..60).filter(|n| n % 5 == s).collect::<Vec<_>>());
    }
}

#[test]
fn async_run_once_requeues_failures() {
    let h = Harness::new();
    h.failures.once.lock().unwrap().insert(0);
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = FakeBrokerAdapter::new();
    broker.deliver("t", &add("a", 0)).unwrap();
    broker.deliver("t", &add("a", 1)).unwrap();
    let first = common::block_on(disp.run_once(&broker, "t", ZERO)).unwrap();
    assert_eq!(first.retried, 1);
    let second = common::block_on(disp.run_once(&broker, "t", ZERO)).unwrap();
    assert_eq!(second.processed, 2);
    assert_eq!(h.seen("a"), [0, 1]);
}

#[test]
fn replay_resolves_a_dead_letter_and_dedups_a_double_replay() {
    let h = Harness::new();
    h.failures.always.lock().unwrap().insert(4);
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let disp = InboundDispatcher::new(machines(), h.handler())
        .max_attempts(1)
        .unwrap()
        .inbox(Arc::new(MemoryInbox::new()))
        .dead_letters(dlq.clone());
    let env = add("q", 4);
    assert_eq!(disp.handle(&env, Some("t")).dead_lettered, 1);
    h.failures.always.lock().unwrap().clear();

    let err = replay_dead_letter(&*dlq, &env.id, &disp, &ReplayOptions::new(" ")).unwrap_err();
    assert_eq!(err.to_string(), "a replay needs a --reason");
    let missing = replay_dead_letter(&*dlq, "nope", &disp, &ReplayOptions::new("fix")).unwrap_err();
    assert_eq!(missing.to_string(), "no dead letter with id 'nope'");

    let dry = replay_dead_letter(&*dlq, &env.id, &disp, &ReplayOptions::new("fixed")).unwrap();
    assert_eq!((dry.dry_run, dry.outcome.as_str()), (true, "would_replay"));
    let real = ReplayOptions { dry_run: false, ..ReplayOptions::new("fixed") };
    let done = replay_dead_letter(&*dlq, &env.id, &disp, &real).unwrap();
    assert_eq!(done.outcome, "processed");
    assert!(dlq.get(&env.id).unwrap().unwrap().resolved_at.is_some());
    let again = replay_dead_letter(&*dlq, &env.id, &disp, &real).unwrap();
    assert_eq!(again.outcome, "duplicate");
    assert_eq!(h.seen("q"), [4]);
}

#[test]
fn replay_refuses_a_changed_machine_unless_forced() {
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let ok = |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> { Ok(ReceiptJson::default()) };
    let mut rec =
        xstate_eda::DeadLetter::new("counter", "", json!({"type": "xsm.counter.ADD"}), 1.0);
    rec.id = "r".into();
    rec.machine_hash = Some("0000000000000000".into());
    rec.machine_version = Some("1.0".into());
    rec.envelope = Some(Value::Object(add("s", 1).to_dict()));
    dlq.put(rec).unwrap();
    let disp = InboundDispatcher::new(machines(), ok);
    let err = replay_dead_letter(&*dlq, "r", &disp, &ReplayOptions::new("why")).unwrap_err();
    let hash = common::counter().structure_hash().to_owned();
    assert_eq!(
        err.to_string(),
        format!(
            "machine structure changed since capture (0000000000000000 != {hash}); machine version \
             changed since capture ('1.0' != None) (use --force to replay anyway)"
        )
    );
    let forced = ReplayOptions { force: true, ..ReplayOptions::new("why") };
    let res = replay_dead_letter(&*dlq, "r", &disp, &forced).unwrap();
    assert_eq!(res.warnings.len(), 2);
    assert_eq!(res.outcome, "would_replay");
}

#[test]
fn dispatcher_records_match_python() {
    let golden = common::golden("dead_letters.json");
    let order = Machine::from_value(&common::golden("asyncapi.json")[0]["config"]).unwrap();
    let dlq = Arc::new(MemoryDeadLetterStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = {
        let calls = Arc::clone(&calls);
        move |_: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(ReceiptJson::default())
        }
    };
    let disp = InboundDispatcher::new(
        machines_by_type(HashMap::from([("xsm.order.PAY".into(), order)])),
        handler,
    )
    .dead_letters(dlq.clone());
    for case in golden["dispatcher"].as_array().unwrap() {
        let expected = &case["record"];
        let env = Envelope::from_dict(&expected["envelope"]).unwrap();
        // the golden envelope is redacted; deliver the original payload
        let mut env = env;
        if env.ty == "xsm.other.GO" {
            env.data = json!({"password": "hunter2", "n": 1});
        }
        let res = disp.handle(&env, Some("orders"));
        assert_eq!(json!(res.outcomes), case["outcomes"]);
        let mut got = dlq.get(&env.id).unwrap().unwrap().to_dict();
        got["taken_at"] = json!(0.0);
        assert_eq!(&got, expected);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

/// The trait objects are usable as declared.
#[test]
fn sinks_are_object_safe() {
    let store: Arc<dyn DeadLetterStore> = Arc::new(MemoryDeadLetterStore::new());
    let sink: Arc<dyn DeadLetterSink> = Arc::new(MemoryDeadLetterStore::new());
    assert_eq!(store.count().unwrap(), 0);
    sink.put(xstate_eda::DeadLetter::new("m", "", json!({}), 0.0)).unwrap();
}

// -- builders and overflow ---------------------------------------------------------------------

#[test]
fn a_zero_batch_is_refused_like_the_other_limits() {
    let h = Harness::new();
    assert!(matches!(
        InboundDispatcher::new(machines(), h.handler()).max_batch(0),
        Err(xstate_eda::EdaError::InvalidArgument(_))
    ));
    assert!(InboundDispatcher::new(machines(), h.handler()).max_batch(1).is_ok());
}

#[test]
fn an_attempt_at_the_ceiling_is_dead_lettered_not_overflowed() {
    let h = Harness::new();
    h.failures.always.lock().unwrap().insert(0);
    let disp = InboundDispatcher::new(machines(), h.handler());
    let env = add("a", 0).with_attempt(u64::MAX);
    let res = disp.handle(&env, None);
    assert_eq!((res.retried, res.dead_lettered), (0, 1));
}

// -- a broker failure in the middle of a batch ---------------------------------------------------

/// A blocking broker that fails on demand: the `n`-th poll, and the ack of chosen envelope ids.
struct FlakySync {
    inner: SyncFakeBrokerAdapter,
    polls: AtomicUsize,
    fail_poll_at: usize,
    fail_ack_of: Mutex<HashSet<String>>,
}

impl FlakySync {
    fn new(fail_poll_at: usize) -> Self {
        Self {
            inner: SyncFakeBrokerAdapter::new(),
            polls: AtomicUsize::new(0),
            fail_poll_at,
            fail_ack_of: Mutex::new(HashSet::new()),
        }
    }
}

impl SyncBrokerAdapter for FlakySync {
    fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        self.inner.publish(topic, envelope)
    }

    fn poll(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        if self.polls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_poll_at {
            return Err(BrokerError::Other("connection lost".into()));
        }
        SyncBrokerAdapter::poll(&self.inner, topic, timeout)
    }

    fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        if self.fail_ack_of.lock().unwrap().contains(&delivery.envelope.id) {
            return Err(BrokerError::Other("ack failed".into()));
        }
        SyncBrokerAdapter::ack(&self.inner, delivery)
    }

    fn nack(&self, delivery: &Delivery, requeue: bool) -> Result<(), BrokerError> {
        SyncBrokerAdapter::nack(&self.inner, delivery, requeue)
    }
}

#[test]
fn a_poll_failure_mid_batch_requeues_what_was_pulled() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = FlakySync::new(3);
    for n in 0..4 {
        broker.inner.deliver("t", &add("a", n)).unwrap();
    }
    let err = disp.run_once_sync(&broker, "t", ZERO).unwrap_err();
    assert_eq!(err, BrokerError::Other("connection lost".into()));
    assert_eq!(broker.inner.in_flight(), 0, "nothing is left holding a delivery");
    assert_eq!(broker.inner.pending("t"), 4, "nothing was lost");
    // the batch comes back in order, so the next consumer sees the subject's events 0, 1, 2, 3
    let res = disp.run_once_sync(&broker.inner, "t", ZERO).unwrap();
    assert_eq!(res.processed, 4);
    assert_eq!(h.seen("a"), [0, 1, 2, 3]);
}

#[test]
fn a_settle_failure_does_not_strand_the_rest_of_the_batch() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = FlakySync::new(usize::MAX);
    let b = add("b", 1);
    broker.fail_ack_of.lock().unwrap().insert(b.id.clone());
    for env in [add("a", 0), b, add("c", 2), add("d", 3)] {
        broker.inner.deliver("t", &env).unwrap();
    }
    let err = disp.run_once_sync(&broker, "t", ZERO).unwrap_err();
    assert_eq!(err, BrokerError::Other("ack failed".into()), "the first failure is returned");
    for (key, n) in [("a", 0), ("c", 2), ("d", 3)] {
        assert_eq!(h.seen(key), [n], "{key} was still handled");
    }
    assert_eq!(broker.inner.acked().len(), 3, "and settled");
    assert_eq!(broker.inner.in_flight(), 1, "only the delivery whose ack failed is left");
}

/// The async counterpart of [`FlakySync`].
struct FlakyAsync {
    inner: FakeBrokerAdapter,
    polls: AtomicUsize,
    fail_poll_at: usize,
    fail_ack_of: Mutex<HashSet<String>>,
}

impl BrokerAdapter for FlakyAsync {
    fn publish(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl std::future::Future<Output = Result<(), BrokerError>> + Send {
        self.inner.publish(topic, envelope)
    }

    async fn poll(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        if self.polls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_poll_at {
            return Err(BrokerError::Other("connection lost".into()));
        }
        BrokerAdapter::poll(&self.inner, topic, timeout).await
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        if self.fail_ack_of.lock().unwrap().contains(&delivery.envelope.id) {
            return Err(BrokerError::Other("ack failed".into()));
        }
        BrokerAdapter::ack(&self.inner, delivery).await
    }

    fn nack(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> impl std::future::Future<Output = Result<(), BrokerError>> + Send {
        BrokerAdapter::nack(&self.inner, delivery, requeue)
    }
}

fn flaky_async(fail_poll_at: usize) -> FlakyAsync {
    FlakyAsync {
        inner: FakeBrokerAdapter::new(),
        polls: AtomicUsize::new(0),
        fail_poll_at,
        fail_ack_of: Mutex::new(HashSet::new()),
    }
}

#[test]
fn an_async_poll_failure_mid_batch_requeues_what_was_pulled() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = flaky_async(3);
    for n in 0..4 {
        broker.inner.deliver("t", &add("a", n)).unwrap();
    }
    let err = common::block_on(disp.run_once(&broker, "t", ZERO)).unwrap_err();
    assert_eq!(err, BrokerError::Other("connection lost".into()));
    assert_eq!((broker.inner.in_flight(), broker.inner.pending("t")), (0, 4));
    let res = common::block_on(disp.run_once(&broker.inner, "t", ZERO)).unwrap();
    assert_eq!(res.processed, 4);
    assert_eq!(h.seen("a"), [0, 1, 2, 3]);
}

#[test]
fn an_async_settle_failure_hands_back_the_unsettled_deliveries() {
    let h = Harness::new();
    let disp = InboundDispatcher::new(machines(), h.handler());
    let broker = flaky_async(usize::MAX);
    let b = add("b", 1);
    broker.fail_ack_of.lock().unwrap().insert(b.id.clone());
    for env in [add("a", 0), b, add("c", 2)] {
        broker.inner.deliver("t", &env).unwrap();
    }
    let err = common::block_on(disp.run_once(&broker, "t", ZERO)).unwrap_err();
    assert_eq!(err, BrokerError::Other("ack failed".into()));
    assert_eq!(broker.inner.in_flight(), 0, "nothing is left holding a delivery");
    assert_eq!(broker.inner.acked().len(), 1, "only a was acked");
    assert_eq!(broker.inner.pending("t"), 2, "b and c are back on the topic");
}
