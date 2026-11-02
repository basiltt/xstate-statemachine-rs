//! Integration tests of the NATS `JetStream` broker.
//!
//! They talk to the server at `XSM_NATS_URL` (default `nats://127.0.0.1:4222`, `JetStream` enabled),
//! use a unique stream per test, remove only their own streams, and skip with a printed note when
//! no server is reachable. The cross-language tests also need the Python virtualenv with
//! `nats-py`.
#![cfg(feature = "nats")]
// The skip notes are printed on purpose: a CI without NATS must say why nothing ran.
#![allow(clippy::print_stderr)]

mod common;

use std::fmt::Display;
use std::future::Future;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_nats::HeaderMap;
use async_nats::header::NATS_MESSAGE_ID;
use async_nats::jetstream::{self, stream::Stream};
use serde_json::json;
use tokio::runtime::Runtime;
use xstate_eda::{
    BrokerError, Delivery, EdaError, Envelope, EnvelopeError, NatsBroker, NatsOptions,
};

const DEFAULT_URL: &str = "nats://127.0.0.1:4222";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn nats_url() -> String {
    std::env::var("XSM_NATS_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

/// A direct `JetStream` connection of the test (to look at, plant and delete messages), driven on a
/// private runtime so it works inside async tests as well.
struct Raw {
    rt: Option<Runtime>,
    js: jetstream::Context,
}

impl Raw {
    fn connect(url: &str) -> Option<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .ok()?;
        let url = url.to_owned();
        // the JetStream context spawns a task: it must be created on the runtime
        let js = block_on(&rt, async move {
            let client =
                tokio::time::timeout(Duration::from_secs(2), async_nats::connect(url)).await;
            client.ok()?.ok().map(jetstream::new)
        });
        // a runtime dropped inside an async test would panic
        let Some(js) = js else {
            rt.shutdown_background();
            return None;
        };
        Some(Self { rt: Some(rt), js })
    }

    fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        block_on(self.rt.as_ref().unwrap_or_else(|| panic!("runtime gone")), fut)
    }

    fn stream(&self, name: &str) -> Stream {
        let (js, name) = (self.js.clone(), name.to_owned());
        ok(self.run(async move { js.get_stream(name).await }))
    }

    /// Messages stored in the stream.
    fn stored(&self, name: &str) -> u64 {
        let stream = self.stream(name);
        ok(self.run(async move { stream.get_info().await })).state.messages
    }

    /// `(delivered but not acked, not yet delivered)` of a durable consumer.
    fn consumer(&self, name: &str, durable: &str) -> (usize, u64) {
        let (stream, durable) = (self.stream(name), durable.to_owned());
        let info = ok(self.run(async move { stream.consumer_info(durable).await }));
        (info.num_ack_pending, info.num_pending)
    }

    /// The stored message with `sequence`: `(subject, headers, payload)`.
    fn message(&self, name: &str, sequence: u64) -> (String, HeaderMap, Vec<u8>) {
        let stream = self.stream(name);
        let msg = ok(self.run(async move { stream.get_raw_message(sequence).await }));
        (msg.subject.to_string(), msg.headers, msg.payload.to_vec())
    }

    /// Store a message the way another writer would.
    fn plant(&self, subject: &str, payload: &[u8]) {
        let (js, subject, payload) = (self.js.clone(), subject.to_owned(), payload.to_vec());
        ok(self.run(async move { js.publish(subject, payload.into()).await?.await.map(|_| ()) }));
    }

    fn delete(&self, name: &str) {
        let (js, name) = (self.js.clone(), name.to_owned());
        let _ = self.run(async move { js.delete_stream(name).await });
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // dropping a runtime inside an async test would panic
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// Run `fut` on `rt` and block until it is done (also from inside another runtime).
fn block_on<F>(rt: &Runtime, fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    drop(rt.spawn(async move {
        let _ = tx.send(fut.await);
    }));
    rx.recv().unwrap_or_else(|e| panic!("task failed: {e}"))
}

/// Retry `check` until it holds (a settle is a message in flight, not a round trip).
fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One test's private stream (`topic`), deleted on drop.
struct Ns {
    url: String,
    topic: String,
    raw: Raw,
}

impl Ns {
    fn new(test: &str) -> Option<Self> {
        let url = nats_url();
        let Some(raw) = Raw::connect(&url) else {
            common::service_unavailable(test, &format!("no NATS at {url} (set XSM_NATS_URL)"));
            return None;
        };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        Some(Self { url, topic: format!("xsmtest{test}{}x{nanos}", std::process::id()), raw })
    }

    fn broker(&self, options: &NatsOptions) -> NatsBroker {
        ok(NatsBroker::open(&self.url, options))
    }

    fn options() -> NatsOptions {
        NatsOptions::new()
    }

    fn pending(&self, durable: &str) -> usize {
        self.raw.consumer(&self.topic, durable).0
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        self.raw.delete(&self.topic);
    }
}

macro_rules! ns {
    ($name:expr) => {
        match Ns::new($name) {
            Some(ns) => ns,
            None => return,
        }
    };
}

fn envelope(subject: &str, n: i64) -> Envelope {
    ok(Envelope::new("xsm.order.GO").subject(subject).data(json!({ "n": n })).build())
}

#[test]
fn options_and_urls_are_validated() {
    let bad = |o: NatsOptions| {
        matches!(NatsBroker::open("nats://127.0.0.1:4222", &o), Err(EdaError::InvalidArgument(_)))
    };
    assert!(bad(NatsOptions::new().durable("")));
    assert!(bad(NatsOptions::new().durable("a.b")));
    assert!(bad(NatsOptions::new().durable("a b")));
    assert!(bad(NatsOptions::new().batch(0)));
    assert!(bad(NatsOptions::new().ack_wait(Duration::ZERO)));
    assert!(bad(NatsOptions::new().io_timeout(Duration::ZERO)));
    assert!(matches!(NatsBroker::open("", &NatsOptions::new()), Err(EdaError::InvalidArgument(_))));
    assert!(NatsBroker::open("nats://a:4222, nats://b:4222", &NatsOptions::new()).is_ok());
}

#[test]
fn a_server_that_never_answers_times_out() {
    use xstate_eda::SyncBrokerAdapter;
    let port = common::silent_server();
    let options = NatsOptions::new().io_timeout(Duration::from_millis(300));
    let broker = ok(NatsBroker::open(&format!("nats://127.0.0.1:{port}"), &options));
    let started = Instant::now();
    let result = SyncBrokerAdapter::publish(&broker, "orders", &envelope("s", 1));
    let Err(BrokerError::Publish(why)) = result else { panic!("expected a timeout: {result:?}") };
    assert!(why.contains("timed out"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(10), "bounded by io_timeout");
    assert!(!broker.healthy());
}

mod sync_api {
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    /// `n` deliveries of `topic`, each polled within three seconds.
    fn take(broker: &NatsBroker, topic: &str, n: usize) -> Vec<Delivery> {
        (0..n)
            .map(|_| {
                ok(broker.poll(topic, Some(Duration::from_secs(3))))
                    .unwrap_or_else(|| panic!("a delivery in time"))
            })
            .collect()
    }

    fn idle(broker: &NatsBroker, topic: &str) -> bool {
        ok(broker.poll(topic, Some(Duration::ZERO))).is_none()
    }

    #[test]
    fn an_unreachable_server_fails_publish_and_marks_the_broker_unhealthy() {
        let broker = ok(NatsBroker::open("nats://127.0.0.1:1", &NatsOptions::new()));
        assert!(broker.healthy());
        let result = broker.publish("orders", &envelope("s", 1));
        assert!(matches!(result, Err(BrokerError::Publish(_))), "{result:?}");
        assert!(!broker.healthy());
        assert!(matches!(broker.poll("orders", Some(Duration::ZERO)), Err(BrokerError::Other(_))));
    }

    #[test]
    fn publish_writes_the_python_stream_subject_and_headers() {
        let ns = ns!("layout");
        let broker = ns.broker(&Ns::options());
        let a = envelope("order.1 *x>", 1);
        let b = ok(Envelope::new("xsm.order.GO").build());
        ok(broker.publish(&ns.topic, &a));
        ok(broker.publish(&ns.topic, &b));

        let info = ok(ns.raw.run({
            let stream = ns.raw.stream(&ns.topic);
            async move { stream.get_info().await }
        }));
        assert_eq!(info.config.subjects, [format!("{}.>", ns.topic)]);
        let (subject, headers, payload) = ns.raw.message(&ns.topic, 1);
        assert_eq!(subject, format!("{}.order_1__x_", ns.topic), "separators and wildcards");
        assert_eq!(String::from_utf8_lossy(&payload), ok(a.to_json(1 << 20)));
        let header = |name: &str| headers.get(name).map(|v| v.as_str().to_owned());
        assert_eq!(
            headers.get(NATS_MESSAGE_ID).map(async_nats::HeaderValue::as_str),
            Some(a.id.as_str())
        );
        assert_eq!(
            header("content-type").as_deref(),
            Some("application/cloudevents+json; charset=UTF-8")
        );
        let (subject, ..) = ns.raw.message(&ns.topic, 2);
        assert_eq!(subject, format!("{}._", ns.topic), "no subject is the token `_`");
    }

    #[test]
    fn the_message_id_header_lets_jetstream_drop_a_republished_envelope() {
        let ns = ns!("dedup");
        let broker = ns.broker(&Ns::options());
        let sent = envelope("s", 1);
        ok(broker.publish(&ns.topic, &sent));
        ok(broker.publish(&ns.topic, &sent));
        assert_eq!(ns.raw.stored(&ns.topic), 1, "the duplicate window dropped the second");
    }

    #[test]
    fn poll_ack_settles_the_message_and_a_second_ack_is_a_no_op() {
        let ns = ns!("ack");
        let broker = ns.broker(&Ns::options());
        let sent = envelope("s", 1);
        assert!(idle(&broker, &ns.topic));
        ok(broker.publish(&ns.topic, &sent));
        let delivery = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(delivery.envelope, sent);
        assert_eq!((delivery.envelope.attempt(), delivery.topic.as_str()), (0, ns.topic.as_str()));
        assert_eq!((ns.pending("xsm"), broker.in_flight()), (1, 1));
        ok(broker.ack(&delivery));
        ok(broker.ack(&delivery));
        eventually("the ack reaches the server", || ns.pending("xsm") == 0);
        assert_eq!(broker.in_flight(), 0);
        assert!(idle(&broker, &ns.topic));
    }

    #[test]
    fn an_idle_poll_waits_for_its_timeout_and_a_blocked_poll_sees_new_traffic() {
        let ns = ns!("idle");
        let broker = ns.broker(&Ns::options());
        let started = Instant::now();
        assert!(ok(broker.poll(&ns.topic, Some(Duration::from_millis(300)))).is_none());
        assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());

        let publisher = ns.broker(&Ns::options().durable("publisher"));
        let (sent, topic) = (envelope("s", 7), ns.topic.clone());
        let to_send = sent.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            ok(publisher.publish(&topic, &to_send));
        });
        let got = ok(broker.poll(&ns.topic, Some(Duration::from_secs(5))))
            .unwrap_or_else(|| panic!("woken by traffic"));
        assert_eq!(got.envelope, sent);
        handle.join().unwrap_or_else(|_| panic!("publisher panicked"));
    }

    #[test]
    fn nack_requeues_at_the_head_with_the_attempt_bumped_and_the_message_stays_pending() {
        let ns = ns!("nack");
        let broker = ns.broker(&Ns::options());
        let (a, b) = (envelope("s", 1), envelope("s", 2));
        ok(broker.publish(&ns.topic, &a));
        ok(broker.publish(&ns.topic, &b));
        let mut first = take(&broker, &ns.topic, 1).remove(0);
        first.envelope.data = json!({"n": 99}); // a consumer mutating its copy changes nothing
        ok(broker.nack(&first, true));
        ok(broker.nack(&first, true)); // settling twice is a no-op
        assert_eq!(ns.pending("xsm"), 2, "nothing was acked");

        let again = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((again.envelope.id.as_str(), again.envelope.attempt()), (a.id.as_str(), 1));
        assert_eq!(again.envelope.data, json!({"n": 1}));
        ok(broker.nack(&again, true));
        let third = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((third.envelope.id.as_str(), third.envelope.attempt()), (a.id.as_str(), 2));
        ok(broker.ack(&third));
        let second = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((second.envelope.id.as_str(), second.envelope.attempt()), (b.id.as_str(), 0));

        // nack without requeue is +TERM: never redelivered
        ok(broker.nack(&second, false));
        eventually("both settled", || ns.pending("xsm") == 0);
        assert!(idle(&broker, &ns.topic));
    }

    #[test]
    fn the_wire_attempt_is_never_trusted() {
        let ns = ns!("wireattempt");
        let broker = ns.broker(&Ns::options());
        ok(broker.publish(&ns.topic, &envelope("s", 1).with_attempt(10)));
        assert_eq!(take(&broker, &ns.topic, 1)[0].envelope.attempt(), 0);
    }

    #[test]
    fn per_subject_order_holds_across_fetches() {
        let ns = ns!("order");
        let broker = ns.broker(&Ns::options().batch(3));
        for n in 0..10 {
            ok(broker.publish(&ns.topic, &envelope("one-subject", n)));
            ok(broker.publish(&ns.topic, &envelope("other", 100 + n)));
        }
        let mut per_subject = std::collections::HashMap::<String, Vec<i64>>::new();
        for delivery in take(&broker, &ns.topic, 20) {
            let subject = delivery.envelope.subject.clone().unwrap_or_default();
            per_subject
                .entry(subject)
                .or_default()
                .push(delivery.envelope.data["n"].as_i64().unwrap_or(-1));
            ok(broker.ack(&delivery));
        }
        assert_eq!(per_subject["one-subject"], (0..10).collect::<Vec<_>>());
        assert_eq!(per_subject["other"], (100..110).collect::<Vec<_>>());
    }

    #[test]
    fn workers_of_one_durable_share_a_topic_and_other_durables_get_their_own_copy() {
        let ns = ns!("group");
        let opts = Ns::options().batch(1);
        let (c1, c2) = (ns.broker(&opts), ns.broker(&opts));
        let mut sent = std::collections::BTreeSet::new();
        for n in 0..6 {
            let e = envelope("s", n);
            sent.insert(e.id.clone());
            ok(c1.publish(&ns.topic, &e));
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            for broker in [&c1, &c2] {
                let d = take(broker, &ns.topic, 1).remove(0);
                ok(broker.ack(&d));
                got.push(d.envelope.id);
            }
        }
        assert_eq!(got.len(), 6);
        assert_eq!(got.iter().cloned().collect::<std::collections::BTreeSet<_>>(), sent);
        assert!(idle(&c1, &ns.topic) && idle(&c2, &ns.topic));

        // another service (durable) reads the whole stream from the start
        let other = ns.broker(&Ns::options().durable("audit"));
        assert_eq!(take(&other, &ns.topic, 6).len(), 6);
    }

    #[test]
    fn an_unacked_message_is_redelivered_by_the_server_with_the_attempt_counted() {
        let ns = ns!("redeliver");
        let opts = Ns::options().ack_wait(Duration::from_secs(1));
        let (doomed, rescuer) = (ns.broker(&opts), ns.broker(&opts));
        let sent = envelope("s", 1);
        ok(doomed.publish(&ns.topic, &sent));
        assert_eq!(take(&doomed, &ns.topic, 1)[0].envelope.attempt(), 0);
        assert!(idle(&rescuer, &ns.topic), "still within ack_wait");
        std::thread::sleep(Duration::from_millis(1300));
        let saved = take(&rescuer, &ns.topic, 1).remove(0);
        assert_eq!((saved.envelope.id.as_str(), saved.envelope.attempt()), (sent.id.as_str(), 1));
        ok(rescuer.ack(&saved));
        eventually("settled", || ns.pending("xsm") == 0);
    }

    #[test]
    fn messages_held_locally_too_long_are_released_to_the_servers_clock() {
        let ns = ns!("release");
        let broker = ns.broker(&Ns::options().ack_wait(Duration::from_secs(1)));
        for n in 0..3 {
            ok(broker.publish(&ns.topic, &envelope("s", n)));
        }
        let first = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(broker.held(&ns.topic), 2, "the batch fetched three, one was delivered");
        std::thread::sleep(Duration::from_millis(600)); // past half the ack wait
        assert!(idle(&broker, &ns.topic), "stale local entries are released, not delivered late");
        assert_eq!(broker.held(&ns.topic), 0);
        std::thread::sleep(Duration::from_millis(600));
        let redelivered = take(&broker, &ns.topic, 2);
        assert!(redelivered.iter().all(|d| d.envelope.attempt() == 1));
        ok(broker.ack(&first));
    }

    #[test]
    fn connect_options_and_a_caller_managed_context_are_supported() {
        let ns = ns!("custom");
        let connects = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&connects);
        let options = Ns::options().connect_with(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async_nats::ConnectOptions::new().name("xsm-test")
        });
        let via_url = ns.broker(&options);
        ok(via_url.publish(&ns.topic, &envelope("s", 1)));
        ok(via_url.publish(&ns.topic, &envelope("s", 2)));
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 1, "one lazy connection");

        let managed = ok(NatsBroker::with_jetstream(ns.raw.js.clone(), &Ns::options()));
        for delivery in take(&managed, &ns.topic, 2) {
            ok(managed.ack(&delivery));
        }
        ok(managed.close_blocking()); // the caller's connection is left open
        ok(managed.publish(&ns.topic, &envelope("s", 3)));
        eventually("acked", || ns.pending("xsm") == 0);
    }
    #[test]
    fn subscribe_ends_after_the_topic_stays_idle() {
        let ns = ns!("subscribe");
        let broker = ns.broker(&Ns::options());
        for n in 0..5 {
            ok(broker.publish(&ns.topic, &envelope("s", n)));
        }
        let mut seen = Vec::new();
        for delivery in broker.subscribe(&ns.topic, Some(Duration::from_millis(500))) {
            let delivery = ok(delivery);
            seen.push(delivery.envelope.data["n"].as_i64().unwrap_or(-1));
            ok(broker.ack(&delivery));
        }
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_size_cap_applies_to_publish_and_to_inbound_messages() {
        let ns = ns!("cap");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = std::sync::Arc::clone(&seen);
        let options =
            Ns::options().max_bytes(400).on_undecodable(move |topic, error: &EnvelopeError| {
                hook_seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((topic.to_owned(), error.type_name()));
            });
        let broker = ns.broker(&options);
        let big = ok(Envelope::new("xsm.o.GO").data(json!({"blob": "x".repeat(1000)})).build());
        assert!(matches!(
            broker.publish(&ns.topic, &big),
            Err(BrokerError::Envelope(EnvelopeError::TooLarge(_)))
        ));

        // messages other writers planted: oversized, not JSON; then a good one
        let good = envelope("s", 1);
        ok(broker.publish(&ns.topic, &good)); // creates the stream
        let subject = format!("{}.planted", ns.topic);
        ns.raw.plant(&subject, "x".repeat(1000).as_bytes());
        ns.raw.plant(&subject, b"not json");
        let delivery = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(delivery.envelope, good);
        assert!(idle(&broker, &ns.topic));
        eventually("the bad messages were terminated", || {
            ns.raw.consumer(&ns.topic, "xsm") == (1, 0)
        });
        let reports = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|(topic, _)| *topic == ns.topic));
        assert_eq!(reports[0].1, "EnvelopeTooLargeError");
    }
}

mod async_api {
    use xstate_eda::BrokerAdapter;

    use super::*;

    async fn poll3(broker: &NatsBroker, topic: &str) -> Result<Option<Delivery>, BrokerError> {
        BrokerAdapter::poll(broker, topic, Some(Duration::from_secs(3))).await
    }

    #[tokio::test]
    async fn publish_poll_nack_ack_work_without_a_caller_runtime_dependency() {
        let ns = ns!("async");
        let broker = ns.broker(&Ns::options());
        let sent = envelope("order:1", 1);
        ok(BrokerAdapter::publish(&broker, &ns.topic, &sent).await);
        let first = ok(poll3(&broker, &ns.topic).await).unwrap_or_else(|| panic!("delivered"));
        assert_eq!(first.envelope, sent);
        ok(BrokerAdapter::nack(&broker, &first, true).await);
        let again = ok(poll3(&broker, &ns.topic).await).unwrap_or_else(|| panic!("redelivered"));
        assert_eq!(again.envelope.attempt(), 1);
        ok(BrokerAdapter::ack(&broker, &again).await);
        eventually("settled", || ns.pending("xsm") == 0);
        assert!(ok(BrokerAdapter::poll(&broker, &ns.topic, Some(Duration::ZERO)).await).is_none());

        // settled through the other trait: one delivery handle works with both
        ok(BrokerAdapter::publish(&broker, &ns.topic, &envelope("s", 2)).await);
        let third = ok(poll3(&broker, &ns.topic).await).unwrap_or_else(|| panic!("delivered"));
        ok(BrokerAdapter::nack(&broker, &third, false).await);
        eventually("terminated", || ns.pending("xsm") == 0);
    }

    #[tokio::test]
    async fn a_poll_dropped_mid_wait_loses_nothing() {
        let ns = ns!("cancel");
        let broker = ns.broker(&Ns::options());
        // cancelled while waiting on the server ...
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            BrokerAdapter::poll(&broker, &ns.topic, Some(Duration::from_secs(30))),
        )
        .await;
        assert!(cancelled.is_err());
        // ... and the message published afterwards is still delivered
        let sent = envelope("s", 1);
        ok(BrokerAdapter::publish(&broker, &ns.topic, &sent).await);
        let got = ok(BrokerAdapter::poll(&broker, &ns.topic, Some(Duration::from_secs(5))).await)
            .unwrap_or_else(|| panic!("delivered"));
        assert_eq!(got.envelope, sent);
    }

    #[tokio::test]
    async fn close_drains_the_connection_and_a_later_call_reconnects() {
        let ns = ns!("close");
        let broker = ns.broker(&Ns::options());
        ok(BrokerAdapter::publish(&broker, &ns.topic, &envelope("s", 1)).await);
        ok(broker.close().await);
        ok(BrokerAdapter::publish(&broker, &ns.topic, &envelope("s", 2)).await);
        assert_eq!(ns.raw.stored(&ns.topic), 2);
    }
}

/// NATS shared with the Python library: each runtime publishes, the other consumes. Skipped (with
/// a note) when the Python virtualenv or its `nats-py` client is missing; override the interpreter
/// with `XSM_PYTHON`.
mod cross_language {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    const SUBJECTS: [&str; 5] = ["order:1", "order.2 x", "ключ-3", "日本/4", "wild*>card"];

    const PY_HEAD: &str = "
import sys, json, asyncio
from xstate_statemachine.contrib.brokers.nats import NatsBroker, subject_token
from xstate_statemachine.eda.envelope import Envelope
url, topic, durable, subjects = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
";

    const PY_PUBLISH: &str = "
async def main():
    broker = NatsBroker(servers=url, durable=durable)
    for i, subject in enumerate(subjects):
        await broker.publish(topic, Envelope.new(type='xsm.order.GO', subject=subject,
                             data={'n': i, 't': '\\u00e9'}, correlationid='corr-' + str(i)))
    print(json.dumps([subject_token(s) for s in subjects]))
    await broker.close()
asyncio.run(main())
";

    const PY_CONSUME: &str = "
async def main():
    broker = NatsBroker(servers=url, durable=durable)
    got = []
    async for d in broker.subscribe(topic, timeout=1.5):
        attempt = d.envelope.attempt
        got.append({'id': d.envelope.id, 'type': d.envelope.type, 'subject': d.envelope.subject,
                    'data': d.envelope.data, 'attempt': attempt() if callable(attempt) else attempt})
        await d.ack()
    print(json.dumps({'got': got, 'tokens': [subject_token(s) for s in subjects]}))
    await broker.close()
asyncio.run(main())
";

    fn python() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("XSM_PYTHON") {
            let p = PathBuf::from(p);
            return p.exists().then_some(p);
        }
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../PythonProjects/xstate-statemachine");
        [".venv/Scripts/python.exe", ".venv/bin/python"]
            .iter()
            .map(|rel| checkout.join(rel))
            .find(|p| p.exists())
    }

    struct Py {
        exe: PathBuf,
        url: String,
    }

    impl Py {
        fn command(&self, script: &str, topic: &str, durable: &str, subjects: &[&str]) -> Command {
            let mut cmd = Command::new(&self.exe);
            cmd.arg("-I")
                .arg("-c")
                .arg(format!("{PY_HEAD}{script}"))
                .args([&self.url, topic, durable])
                .arg(json!(subjects).to_string());
            cmd
        }

        fn run(&self, script: &str, topic: &str, durable: &str, subjects: &[&str]) -> Value {
            let out = self
                .command(script, topic, durable, subjects)
                .output()
                .unwrap_or_else(|e| panic!("python: {e}"));
            assert!(
                out.status.success(),
                "python failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            ok(serde_json::from_slice(&out.stdout))
        }
    }

    fn usable_python(ns: &Ns) -> Option<Py> {
        let Some(exe) = python() else {
            eprintln!("skipping NATS cross-language test: no Python virtualenv (set XSM_PYTHON)");
            return None;
        };
        let py = Py { exe, url: ns.url.clone() };
        let probe = py.command("", "t", "d", &[]).output().ok()?;
        if probe.status.success() {
            return Some(py);
        }
        eprintln!(
            "skipping NATS cross-language test: {}",
            String::from_utf8_lossy(&probe.stderr).lines().last().unwrap_or("python import failed")
        );
        None
    }

    #[test]
    fn python_publishes_rust_consumes_then_rust_publishes_python_consumes() {
        let ns = ns!("xlang");
        let Some(py) = usable_python(&ns) else { return };

        // Python -> Rust: Python creates the stream, Rust creates its consumer on it
        let topic_a = format!("{}a", ns.topic);
        let tokens = py.run(PY_PUBLISH, &topic_a, "xl", &SUBJECTS);
        let rust = ns.broker(&Ns::options().durable("xl"));
        let mut got = Vec::new();
        while let Some(d) = ok(rust.poll(&topic_a, Some(Duration::from_millis(500)))) {
            assert_eq!(d.envelope.attempt(), 0);
            ok(rust.ack(&d));
            got.push(d.envelope);
        }
        assert_eq!(got.len(), SUBJECTS.len());
        for (i, subject) in SUBJECTS.iter().enumerate() {
            let e = &got[i]; // one durable, one stream: publish order
            assert_eq!(e.subject.as_deref(), Some(*subject));
            assert_eq!((e.ty.as_str(), &e.data), ("xsm.order.GO", &json!({"n": i, "t": "é"})));
            assert_eq!(e.correlationid.as_deref(), Some(format!("corr-{i}").as_str()));
            let (stored, headers, _) = ns.raw.message(&topic_a, u64::try_from(i + 1).unwrap_or(0));
            assert_eq!(stored, format!("{topic_a}.{}", tokens[i].as_str().unwrap_or("?")));
            assert_eq!(
                headers.get(NATS_MESSAGE_ID).map(async_nats::HeaderValue::as_str),
                Some(e.id.as_str())
            );
        }
        eventually("Python-published messages acked", || ns.raw.consumer(&topic_a, "xl") == (0, 0));
        ns.raw.delete(&topic_a);

        // Rust -> Python
        let topic_b = format!("{}b", ns.topic);
        let sent: Vec<Envelope> =
            SUBJECTS.iter().zip(0..).map(|(subject, n)| envelope(subject, n)).collect();
        for e in &sent {
            ok(rust.publish(&topic_b, e));
        }
        let seen = py.run(PY_CONSUME, &topic_b, "py", &SUBJECTS);
        let consumed = seen["got"].as_array().unwrap_or_else(|| panic!("list"));
        assert_eq!(consumed.len(), sent.len());
        for (i, e) in sent.iter().enumerate() {
            assert_eq!(consumed[i]["id"], e.id.as_str(), "publish order");
            assert_eq!(consumed[i]["type"], e.ty.as_str());
            assert_eq!(consumed[i]["subject"], e.subject.as_deref().unwrap_or_default());
            assert_eq!(consumed[i]["data"], e.data);
            assert_eq!(consumed[i]["attempt"], 0);
            let (stored, ..) = ns.raw.message(&topic_b, u64::try_from(i + 1).unwrap_or(0));
            assert_eq!(stored, format!("{topic_b}.{}", seen["tokens"][i].as_str().unwrap_or("?")));
        }
        eventually("Rust-published messages acked by Python", || {
            ns.raw.consumer(&topic_b, "py") == (0, 0)
        });
        ns.raw.delete(&topic_b);
    }

    #[test]
    fn rust_joins_a_consumer_python_created() {
        let ns = ns!("xjoin");
        let Some(py) = usable_python(&ns) else { return };
        // Python creates the stream and the durable consumer `shared` (nothing to consume yet)
        py.run(PY_CONSUME, &ns.topic, "shared", &SUBJECTS);
        let rust = ns.broker(&Ns::options().durable("shared"));
        let sent = envelope("order:9", 9);
        ok(rust.publish(&ns.topic, &sent));
        let d = ok(rust.poll(&ns.topic, Some(Duration::from_secs(3))))
            .unwrap_or_else(|| panic!("delivered"));
        assert_eq!(d.envelope, sent);
        ok(rust.ack(&d));
        eventually("acked", || ns.raw.consumer(&ns.topic, "shared") == (0, 0));
    }
}
