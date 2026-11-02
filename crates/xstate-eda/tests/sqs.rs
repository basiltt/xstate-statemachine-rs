//! Integration tests of the Amazon SQS broker.
//!
//! They talk to the SQS-compatible endpoint at `XSM_SQS_ENDPOINT` (default
//! `http://127.0.0.1:9324`, e.g. `docker run -p 9324:9324 softwaremill/elasticmq-native`) with
//! dummy credentials, use a unique queue per test, delete only their own queues, and skip with a
//! printed note when no endpoint is reachable. The cross-language tests also need the Python
//! virtualenv with `boto3`.
#![cfg(feature = "sqs")]
// The skip notes are printed on purpose: a CI without an SQS endpoint must say why nothing ran.
#![allow(clippy::print_stderr)]

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_sdk_sqs::Client;
use aws_sdk_sqs::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_sqs::types::{MessageAttributeValue, MessageSystemAttributeName, QueueAttributeName};
use serde_json::json;
use tokio::runtime::Runtime;
use xstate_eda::{BrokerError, Delivery, EdaError, Envelope, EnvelopeError, SqsBroker, SqsOptions};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9324";
const REGION: &str = "us-east-1";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn endpoint() -> String {
    std::env::var("XSM_SQS_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned())
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

/// Retry `check` until it holds (a settle is a request in flight, not always a round trip).
fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What a raw receive saw of one stored message.
struct Stored {
    body: String,
    system: HashMap<MessageSystemAttributeName, String>,
    attributes: HashMap<String, String>,
}

/// One test's private queues and a direct SDK client (to create, look at, plant into and delete
/// queues), driven on a private runtime so it works inside async tests as well.
struct Qs {
    rt: Option<Runtime>,
    client: Client,
    endpoint: String,
    /// The test's default queue name.
    topic: String,
    urls: std::sync::Mutex<HashMap<String, String>>,
}

impl Qs {
    fn new(test: &str) -> Option<Self> {
        let endpoint = endpoint();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .ok()?;
        let config = aws_sdk_sqs::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(REGION))
            .credentials_provider(Credentials::new("test", "test", None, None, "xsm-test"))
            .endpoint_url(&endpoint)
            .build();
        let client = Client::from_conf(config);
        let probe = client.clone();
        let reachable = block_on(&rt, async move {
            tokio::time::timeout(Duration::from_secs(3), probe.list_queues().send())
                .await
                .is_ok_and(|r| r.is_ok())
        });
        if !reachable {
            // a runtime dropped inside an async test would panic
            rt.shutdown_background();
            eprintln!("skipping `{test}`: no SQS endpoint at {endpoint} (set XSM_SQS_ENDPOINT)");
            return None;
        }
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let topic = format!("xsmtest{test}{}x{nanos}", std::process::id());
        let qs = Self { rt: Some(rt), client, endpoint, topic, urls: std::sync::Mutex::default() };
        qs.create(&qs.topic, false);
        Some(qs)
    }

    fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        block_on(self.rt.as_ref().unwrap_or_else(|| panic!("runtime gone")), fut)
    }

    /// Create queue `name` (a FIFO queue when `fifo`) and remember to delete it.
    fn create(&self, name: &str, fifo: bool) {
        let client = self.client.clone();
        let mut request = client.create_queue().queue_name(name);
        if fifo {
            request = request.attributes(QueueAttributeName::FifoQueue, "true");
        }
        let output = ok(self.run(async move { request.send().await }));
        let url = output.queue_url().unwrap_or_default().to_owned();
        self.urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), url);
    }

    fn url(&self, name: &str) -> String {
        let urls = self.urls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        urls.get(name).cloned().unwrap_or_else(|| panic!("unknown queue {name}"))
    }

    fn broker(&self, options: &SqsOptions) -> SqsBroker {
        ok(SqsBroker::open(&self.options(options.clone())))
    }

    /// `options` pointed at this endpoint.
    fn options(&self, options: SqsOptions) -> SqsOptions {
        options.endpoint_url(&self.endpoint).region(REGION).credentials("test", "test")
    }

    /// `(visible, in flight)` messages of queue `name`.
    fn counts(&self, name: &str) -> (u64, u64) {
        let (client, url) = (self.client.clone(), self.url(name));
        let output = ok(self.run(async move {
            client
                .get_queue_attributes()
                .queue_url(url)
                .attribute_names(QueueAttributeName::All)
                .send()
                .await
        }));
        let attributes = output.attributes().cloned().unwrap_or_default();
        let count = |key: QueueAttributeName| {
            attributes.get(&key).and_then(|v| v.parse().ok()).unwrap_or(0)
        };
        (
            count(QueueAttributeName::ApproximateNumberOfMessages),
            count(QueueAttributeName::ApproximateNumberOfMessagesNotVisible),
        )
    }

    fn in_flight(&self, name: &str) -> u64 {
        self.counts(name).1
    }

    /// Receive what is stored in `name`, leaving it visible (the receive count still grows).
    fn peek(&self, name: &str) -> Vec<Stored> {
        let (client, url) = (self.client.clone(), self.url(name));
        let output = ok(self.run(async move {
            client
                .receive_message()
                .queue_url(url)
                .max_number_of_messages(10)
                .visibility_timeout(0)
                .message_system_attribute_names(MessageSystemAttributeName::All)
                .message_attribute_names("All")
                .send()
                .await
        }));
        output
            .messages()
            .iter()
            .map(|m| Stored {
                body: m.body().unwrap_or_default().to_owned(),
                system: m.attributes().cloned().unwrap_or_default(),
                attributes: m
                    .message_attributes()
                    .map(|a| {
                        a.iter()
                            .map(|(k, v)| {
                                (k.clone(), v.string_value().unwrap_or_default().to_owned())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// Store a message the way another writer would.
    fn plant(&self, name: &str, body: &str) {
        let (client, url, body) = (self.client.clone(), self.url(name), body.to_owned());
        let attribute =
            ok(MessageAttributeValue::builder().data_type("String").string_value("x").build());
        ok(self.run(async move {
            client
                .send_message()
                .queue_url(url)
                .message_body(body)
                .message_attributes("other", attribute)
                .send()
                .await
        }));
    }
}

impl Drop for Qs {
    fn drop(&mut self) {
        let urls: Vec<String> = self
            .urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for url in urls {
            let client = self.client.clone();
            let _ = self.run(async move { client.delete_queue().queue_url(url).send().await });
        }
        // dropping a runtime inside an async test would panic
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

macro_rules! qs {
    ($name:expr) => {
        match Qs::new($name) {
            Some(qs) => qs,
            None => return,
        }
    };
}

fn envelope(subject: &str, n: i64) -> Envelope {
    ok(Envelope::new("xsm.order.GO").subject(subject).data(json!({ "n": n })).build())
}

#[test]
fn options_are_validated_and_debug_hides_secrets() {
    let bad = |o: SqsOptions| matches!(SqsBroker::open(&o), Err(EdaError::InvalidArgument(_)));
    assert!(bad(SqsOptions::new().batch(0)));
    assert!(bad(SqsOptions::new().region("")));
    assert!(bad(SqsOptions::new().endpoint_url("not a url")));
    assert!(bad(SqsOptions::new().visibility_timeout(Duration::from_secs(50_000))));
    let shown = format!(
        "{:?}",
        SqsOptions::new().endpoint_url("http://sekrit-host:9324").credentials("AKIDSEKRIT", "sk")
    );
    assert!(!shown.contains("sekrit") && !shown.contains("AKIDSEKRIT"), "{shown}");
}

mod sync_api {
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    /// `n` deliveries of `topic`, each polled within five seconds.
    fn take(broker: &SqsBroker, topic: &str, n: usize) -> Vec<Delivery> {
        (0..n)
            .map(|_| {
                ok(broker.poll(topic, Some(Duration::from_secs(5))))
                    .unwrap_or_else(|| panic!("a delivery in time"))
            })
            .collect()
    }

    fn idle(broker: &SqsBroker, topic: &str) -> bool {
        ok(broker.poll(topic, Some(Duration::ZERO))).is_none()
    }

    #[test]
    fn an_unreachable_endpoint_fails_publish_without_echoing_it_and_marks_the_broker_unhealthy() {
        let options = SqsOptions::new()
            .endpoint_url("http://127.0.0.1:1")
            .region(REGION)
            .credentials("test", "test");
        let broker = ok(SqsBroker::open(&options));
        assert!(broker.healthy());
        let result = broker.publish("orders", &envelope("s", 1));
        let Err(BrokerError::Publish(why)) = &result else { panic!("{result:?}") };
        assert!(!why.contains("127.0.0.1") && !why.contains("test"), "{why}");
        assert!(!broker.healthy());
        assert!(matches!(broker.poll("orders", Some(Duration::ZERO)), Err(BrokerError::Other(_))));
    }

    #[test]
    fn a_missing_queue_is_an_error_not_created() {
        let qs = qs!("missing");
        let broker = qs.broker(&SqsOptions::new());
        let result = broker.publish("xsmtest-no-such-queue", &envelope("s", 1));
        assert!(matches!(result, Err(BrokerError::Publish(_))), "{result:?}");
        assert!(matches!(
            broker.poll("xsmtest-no-such-queue", Some(Duration::ZERO)),
            Err(BrokerError::Other(_))
        ));
    }

    #[test]
    fn publish_writes_the_python_body_and_ce_type_attribute() {
        let qs = qs!("layout");
        let broker = qs.broker(&SqsOptions::new());
        let sent = envelope("order.1 *x>", 1);
        ok(broker.publish(&qs.topic, &sent));
        let stored = qs.peek(&qs.topic);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, ok(sent.to_json(1 << 20)));
        assert_eq!(stored[0].attributes.get("ce_type").map(String::as_str), Some("xsm.order.GO"));
        assert!(!stored[0].system.contains_key(&MessageSystemAttributeName::MessageGroupId));
    }

    #[test]
    fn fifo_queues_get_the_subject_as_group_and_the_envelope_id_as_dedup_id() {
        let qs = qs!("fifo");
        let fifo = format!("{}.fifo", qs.topic);
        qs.create(&fifo, true);
        let broker = qs.broker(&SqsOptions::new());
        let (a, b) = (envelope("order-1", 1), ok(Envelope::new("xsm.order.GO").build()));
        ok(broker.publish(&fifo, &a));
        ok(broker.publish(&fifo, &a)); // the dedup window drops the re-publish
        ok(broker.publish(&fifo, &b));
        let stored = qs.peek(&fifo);
        assert_eq!(stored.len(), 2, "the duplicate was dropped");
        let group = |s: &Stored| s.system.get(&MessageSystemAttributeName::MessageGroupId).cloned();
        let first = stored
            .iter()
            .find(|s| group(s).as_deref() == Some("order-1"))
            .unwrap_or_else(|| panic!("a message in group order-1"));
        assert_eq!(
            first.system.get(&MessageSystemAttributeName::MessageDeduplicationId),
            Some(&a.id)
        );
        assert!(stored.iter().any(|s| group(s).as_deref() == Some("_")), "no subject = group `_`");
        // consuming: per-group order, the duplicate gone, the subject-less one in group `_`
        let mut seen = Vec::new();
        for _ in 0..2 {
            let d = take(&broker, &fifo, 1).remove(0);
            seen.push(d.envelope.id.clone());
            ok(broker.ack(&d));
        }
        seen.sort();
        let mut expected = vec![a.id.clone(), b.id.clone()];
        expected.sort();
        assert_eq!(seen, expected);
        eventually("drained", || qs.counts(&fifo) == (0, 0));
    }

    #[test]
    fn poll_ack_deletes_the_message_and_a_second_ack_is_a_no_op() {
        let qs = qs!("ack");
        let broker = qs.broker(&SqsOptions::new());
        let sent = envelope("s", 1);
        assert!(idle(&broker, &qs.topic));
        ok(broker.publish(&qs.topic, &sent));
        let delivery = take(&broker, &qs.topic, 1).remove(0);
        assert_eq!(delivery.envelope, sent);
        assert_eq!((delivery.envelope.attempt(), delivery.topic.as_str()), (0, qs.topic.as_str()));
        assert_eq!((qs.in_flight(&qs.topic), broker.in_flight()), (1, 1));
        ok(broker.ack(&delivery));
        ok(broker.ack(&delivery));
        eventually("the message is deleted", || qs.counts(&qs.topic) == (0, 0));
        assert_eq!(broker.in_flight(), 0);
        assert!(idle(&broker, &qs.topic));
    }

    #[test]
    fn an_idle_poll_waits_for_its_timeout_and_a_blocked_poll_sees_new_traffic() {
        let qs = qs!("idle");
        let broker = qs.broker(&SqsOptions::new());
        let started = Instant::now();
        assert!(ok(broker.poll(&qs.topic, Some(Duration::from_millis(300)))).is_none());
        assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

        let publisher = qs.broker(&SqsOptions::new());
        let (sent, topic) = (envelope("s", 7), qs.topic.clone());
        let to_send = sent.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            ok(publisher.publish(&topic, &to_send));
        });
        let got = ok(broker.poll(&qs.topic, Some(Duration::from_secs(5))))
            .unwrap_or_else(|| panic!("woken by traffic"));
        assert_eq!(got.envelope, sent);
        handle.join().unwrap_or_else(|_| panic!("publisher panicked"));
    }

    #[test]
    fn nack_requeues_at_the_head_with_the_attempt_bumped_and_the_message_stays_in_flight() {
        let qs = qs!("nack");
        let broker = qs.broker(&SqsOptions::new().batch(10));
        let (a, b) = (envelope("s", 1), envelope("s", 2));
        ok(broker.publish(&qs.topic, &a));
        ok(broker.publish(&qs.topic, &b));
        let mut first = take(&broker, &qs.topic, 1).remove(0);
        let first_id = first.envelope.id.clone();
        first.envelope.data = json!({"n": 99}); // a consumer mutating its copy changes nothing
        ok(broker.nack(&first, true));
        ok(broker.nack(&first, true)); // settling twice is a no-op
        assert_eq!(qs.counts(&qs.topic), (0, 2), "nothing was deleted");

        let again = take(&broker, &qs.topic, 1).remove(0);
        assert_eq!((again.envelope.id.as_str(), again.envelope.attempt()), (first_id.as_str(), 1));
        assert_ne!(again.envelope.data, json!({"n": 99}));
        ok(broker.nack(&again, true));
        let third = take(&broker, &qs.topic, 1).remove(0);
        assert_eq!((third.envelope.id.as_str(), third.envelope.attempt()), (first_id.as_str(), 2));
        ok(broker.ack(&third));
        let second = take(&broker, &qs.topic, 1).remove(0);
        assert_ne!(second.envelope.id, first_id);
        assert_eq!(second.envelope.attempt(), 0);

        // nack without requeue deletes: never redelivered
        ok(broker.nack(&second, false));
        eventually("both deleted", || qs.counts(&qs.topic) == (0, 0));
        assert!(idle(&broker, &qs.topic));
    }

    #[test]
    fn the_wire_attempt_is_never_trusted() {
        let qs = qs!("wireattempt");
        let broker = qs.broker(&SqsOptions::new());
        ok(broker.publish(&qs.topic, &envelope("s", 1).with_attempt(10)));
        assert_eq!(take(&broker, &qs.topic, 1)[0].envelope.attempt(), 0);
    }

    #[test]
    fn the_attempt_is_the_approximate_receive_count_minus_one() {
        let qs = qs!("count");
        let broker = qs.broker(&SqsOptions::new());
        ok(broker.publish(&qs.topic, &envelope("s", 1)));
        // two receives by somebody else (each leaves it visible again) = two earlier deliveries
        assert_eq!(qs.peek(&qs.topic).len(), 1);
        assert_eq!(qs.peek(&qs.topic).len(), 1);
        assert_eq!(take(&broker, &qs.topic, 1)[0].envelope.attempt(), 2);
    }

    #[test]
    fn an_undeleted_message_is_redelivered_after_the_visibility_timeout_with_the_attempt_counted() {
        let qs = qs!("redeliver");
        let opts = SqsOptions::new().visibility_timeout(Duration::from_secs(1));
        let (doomed, rescuer) = (qs.broker(&opts), qs.broker(&opts));
        let sent = envelope("s", 1);
        ok(doomed.publish(&qs.topic, &sent));
        assert_eq!(take(&doomed, &qs.topic, 1)[0].envelope.attempt(), 0);
        assert!(idle(&rescuer, &qs.topic), "still invisible");
        std::thread::sleep(Duration::from_millis(1300));
        let saved = take(&rescuer, &qs.topic, 1).remove(0);
        assert_eq!((saved.envelope.id.as_str(), saved.envelope.attempt()), (sent.id.as_str(), 1));
        std::thread::sleep(Duration::from_millis(1300));
        let again = take(&doomed, &qs.topic, 1).remove(0);
        assert_eq!(again.envelope.attempt(), 2);
        ok(doomed.ack(&again));
        eventually("deleted", || qs.counts(&qs.topic) == (0, 0));
    }

    #[test]
    fn messages_held_locally_too_long_are_released_to_the_visibility_clock() {
        let qs = qs!("release");
        let broker = qs.broker(&SqsOptions::new().visibility_timeout(Duration::from_secs(2)));
        for n in 0..3 {
            ok(broker.publish(&qs.topic, &envelope("s", n)));
        }
        let _first = take(&broker, &qs.topic, 1).remove(0);
        assert_eq!(broker.held(&qs.topic), 2, "the batch received three, one was delivered");
        std::thread::sleep(Duration::from_millis(1200)); // past half the visibility timeout
        assert!(idle(&broker, &qs.topic), "stale local entries are released, not delivered late");
        assert_eq!(broker.held(&qs.topic), 0);
        std::thread::sleep(Duration::from_millis(1200));
        let redelivered = take(&broker, &qs.topic, 2);
        assert!(redelivered.iter().all(|d| d.envelope.attempt() == 1));
    }

    #[test]
    fn extend_visibility_keeps_a_delivery_hidden_longer() {
        let qs = qs!("extend");
        let opts = SqsOptions::new().visibility_timeout(Duration::from_secs(1));
        let (holder, other) = (qs.broker(&opts), qs.broker(&opts));
        ok(holder.publish(&qs.topic, &envelope("s", 1)));
        let delivery = take(&holder, &qs.topic, 1).remove(0);
        ok(holder.extend_visibility_blocking(&delivery, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(idle(&other, &qs.topic), "extended past the original timeout");
        ok(holder.ack(&delivery));
        ok(holder.extend_visibility_blocking(&delivery, Duration::from_secs(30))); // settled: no-op
        eventually("deleted", || qs.counts(&qs.topic) == (0, 0));
    }

    #[test]
    fn per_subject_order_holds_across_receives_on_a_fifo_queue() {
        let qs = qs!("order");
        let fifo = format!("{}.fifo", qs.topic);
        qs.create(&fifo, true);
        let broker = qs.broker(&SqsOptions::new().batch(3));
        for n in 0..8 {
            ok(broker.publish(&fifo, &envelope("one-subject", n)));
        }
        let mut seen = Vec::new();
        // a FIFO group hands out its next messages only once the earlier ones are deleted
        for _ in 0..8 {
            let delivery = take(&broker, &fifo, 1).remove(0);
            seen.push(delivery.envelope.data["n"].as_i64().unwrap_or(-1));
            ok(broker.ack(&delivery));
        }
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn workers_sharing_a_queue_split_its_messages() {
        let qs = qs!("group");
        let opts = SqsOptions::new().batch(1);
        let (c1, c2) = (qs.broker(&opts), qs.broker(&opts));
        let mut sent = std::collections::BTreeSet::new();
        for n in 0..6 {
            let e = envelope("s", n);
            sent.insert(e.id.clone());
            ok(c1.publish(&qs.topic, &e));
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            for broker in [&c1, &c2] {
                let d = take(broker, &qs.topic, 1).remove(0);
                ok(broker.ack(&d));
                got.push(d.envelope.id);
            }
        }
        assert_eq!(got.len(), 6);
        assert_eq!(got.iter().cloned().collect::<std::collections::BTreeSet<_>>(), sent);
        assert!(idle(&c1, &qs.topic) && idle(&c2, &qs.topic));
    }

    #[test]
    fn subscribe_ends_after_the_topic_stays_idle() {
        let qs = qs!("subscribe");
        let broker = qs.broker(&SqsOptions::new());
        for n in 0..5 {
            ok(broker.publish(&qs.topic, &envelope("s", n)));
        }
        let mut seen = Vec::new();
        for delivery in broker.subscribe(&qs.topic, Some(Duration::from_millis(500))) {
            let delivery = ok(delivery);
            seen.push(delivery.envelope.data["n"].as_i64().unwrap_or(-1));
            ok(broker.ack(&delivery));
        }
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_size_cap_applies_to_publish_and_to_inbound_messages() {
        let qs = qs!("cap");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = std::sync::Arc::clone(&seen);
        let options =
            SqsOptions::new().max_bytes(400).on_undecodable(move |topic, error: &EnvelopeError| {
                hook_seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((topic.to_owned(), error.type_name()));
            });
        let broker = qs.broker(&options);
        let big = ok(Envelope::new("xsm.o.GO").data(json!({"blob": "x".repeat(1000)})).build());
        assert!(matches!(
            broker.publish(&qs.topic, &big),
            Err(BrokerError::Envelope(EnvelopeError::TooLarge(_)))
        ));
        assert_eq!(qs.counts(&qs.topic), (0, 0), "nothing was sent");

        // messages other writers planted: oversized, not JSON, wrong shape; then a good one
        qs.plant(&qs.topic, &"x".repeat(1000));
        qs.plant(&qs.topic, "not json");
        let good = envelope("s", 1);
        ok(broker.publish(&qs.topic, &good));
        let delivery = take(&broker, &qs.topic, 1).remove(0);
        assert_eq!(delivery.envelope, good);
        assert!(idle(&broker, &qs.topic));
        eventually("the bad messages were deleted", || qs.counts(&qs.topic) == (0, 1));
        let reports = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|(topic, _)| *topic == qs.topic));
        assert!(reports.iter().any(|(_, kind)| *kind == "EnvelopeTooLargeError"));
    }

    #[test]
    fn a_caller_built_client_is_supported_and_close_reconnects() {
        let qs = qs!("custom");
        let broker = ok(SqsBroker::with_client(qs.client.clone(), &SqsOptions::new()));
        ok(broker.publish(&qs.topic, &envelope("s", 1)));
        ok(broker.close_blocking()); // the caller's client is left alone
        let d = take(&broker, &qs.topic, 1).remove(0);
        ok(broker.ack(&d));
        let own = qs.broker(&SqsOptions::new());
        ok(own.publish(&qs.topic, &envelope("s", 2)));
        ok(own.close_blocking());
        ok(own.publish(&qs.topic, &envelope("s", 3))); // a later call builds a new client
    }
}

mod async_api {
    use xstate_eda::BrokerAdapter;

    use super::*;

    async fn poll5(broker: &SqsBroker, topic: &str) -> Result<Option<Delivery>, BrokerError> {
        BrokerAdapter::poll(broker, topic, Some(Duration::from_secs(5))).await
    }

    #[tokio::test]
    async fn publish_poll_nack_ack_work_without_a_caller_runtime_dependency() {
        let qs = qs!("async");
        let broker = qs.broker(&SqsOptions::new());
        let sent = envelope("order:1", 1);
        ok(BrokerAdapter::publish(&broker, &qs.topic, &sent).await);
        let first = ok(poll5(&broker, &qs.topic).await).unwrap_or_else(|| panic!("delivered"));
        assert_eq!(first.envelope, sent);
        ok(BrokerAdapter::nack(&broker, &first, true).await);
        let again = ok(poll5(&broker, &qs.topic).await).unwrap_or_else(|| panic!("redelivered"));
        assert_eq!(again.envelope.attempt(), 1);
        ok(BrokerAdapter::ack(&broker, &again).await);
        eventually("deleted", || qs.counts(&qs.topic) == (0, 0));
        assert!(ok(BrokerAdapter::poll(&broker, &qs.topic, Some(Duration::ZERO)).await).is_none());

        ok(BrokerAdapter::publish(&broker, &qs.topic, &envelope("s", 2)).await);
        let third = ok(poll5(&broker, &qs.topic).await).unwrap_or_else(|| panic!("delivered"));
        ok(broker.extend_visibility(&third, Duration::from_secs(60)).await);
        ok(BrokerAdapter::nack(&broker, &third, false).await);
        eventually("deleted", || qs.counts(&qs.topic) == (0, 0));
    }

    #[tokio::test]
    async fn a_poll_dropped_mid_wait_loses_nothing() {
        let qs = qs!("cancel");
        let broker = qs.broker(&SqsOptions::new());
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            BrokerAdapter::poll(&broker, &qs.topic, Some(Duration::from_secs(30))),
        )
        .await;
        assert!(cancelled.is_err());
        let sent = envelope("s", 1);
        ok(BrokerAdapter::publish(&broker, &qs.topic, &sent).await);
        let got = ok(poll5(&broker, &qs.topic).await).unwrap_or_else(|| panic!("delivered"));
        assert_eq!(got.envelope, sent);
    }
}

#[path = "sqs/cross_language.rs"]
mod cross_language;
