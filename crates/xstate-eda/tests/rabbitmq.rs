//! Integration tests of the RabbitMQ broker.
//!
//! They talk to the broker at `XSM_AMQP_URL` (default `amqp://guest:guest@127.0.0.1:5672/%2f`), use
//! a unique queue per test, remove only their own queues and exchanges, and skip with a printed note
//! when no broker is reachable. The cross-language tests also need the Python virtualenv with
//! `aio-pika`.
#![cfg(feature = "rabbitmq")]
// The skip notes are printed on purpose: a CI without RabbitMQ must say why nothing ran.
#![allow(clippy::print_stderr)]

mod common;

use std::fmt::Display;
use std::future::Future;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lapin::options::{
    BasicGetOptions, BasicPublishOptions, ExchangeDeclareOptions, ExchangeDeleteOptions,
    QueueBindOptions, QueueDeclareOptions, QueueDeleteOptions,
};
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind};
use serde_json::json;
use tokio::runtime::Runtime;
use xstate_eda::{
    BrokerError, Delivery, EdaError, Envelope, EnvelopeError, RabbitMqBroker, RabbitMqOptions,
};

const DEFAULT_URL: &str = "amqp://guest:guest@127.0.0.1:5672/%2f";
const CE_CONTENT_TYPE: &str = "application/cloudevents+json; charset=UTF-8";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn amqp_url() -> String {
    std::env::var("XSM_AMQP_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

/// A message as a raw consumer sees it.
struct Got {
    properties: BasicProperties,
    body: Vec<u8>,
    redelivered: bool,
}

impl Got {
    fn header(&self, name: &str) -> Option<String> {
        match self.properties.headers().as_ref()?.inner().get(name)? {
            AMQPValue::LongString(s) => Some(s.to_string()),
            _ => None,
        }
    }

    fn text(field: Option<&lapin::types::ShortString>) -> Option<String> {
        field.map(|s| s.as_str().to_owned())
    }
}

/// A direct AMQP connection of the test (to look at, plant and delete messages), driven on a
/// private runtime so it works inside async tests as well.
struct Raw {
    rt: Option<Runtime>,
    conn: std::sync::Arc<Connection>,
}

impl Raw {
    fn connect(url: &str) -> Option<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .ok()?;
        let url = url.to_owned();
        let conn = block_on(&rt, async move {
            let connecting = Connection::connect(&url, ConnectionProperties::default());
            tokio::time::timeout(Duration::from_secs(3), connecting)
                .await
                .ok()?
                .ok()
                .map(std::sync::Arc::new)
        });
        // a runtime dropped inside an async test would panic
        let Some(conn) = conn else {
            rt.shutdown_background();
            return None;
        };
        Some(Self { rt: Some(rt), conn })
    }

    fn run<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        block_on(self.rt.as_ref().unwrap_or_else(|| panic!("runtime gone")), fut)
    }

    /// A fresh channel (a failing operation closes its channel).
    fn channel(&self) -> Channel {
        let conn = std::sync::Arc::clone(&self.conn);
        ok(self.run(async move { conn.create_channel().await }))
    }

    /// Ready messages of a queue; `None` when it does not exist.
    fn count(&self, queue: &str) -> Option<u32> {
        let (channel, queue) = (self.channel(), queue.to_owned());
        let options = QueueDeclareOptions { passive: true, ..QueueDeclareOptions::default() };
        self.run(async move {
            let q = channel.queue_declare(&queue, options, FieldTable::default()).await.ok()?;
            Some(q.message_count())
        })
    }

    fn declare(&self, queue: &str) {
        let (channel, queue) = (self.channel(), queue.to_owned());
        let options = QueueDeclareOptions { durable: true, ..QueueDeclareOptions::default() };
        ok(self.run(
            async move { channel.queue_declare(&queue, options, FieldTable::default()).await },
        ));
    }

    /// Take (and ack) the next message of a queue.
    fn get(&self, queue: &str) -> Option<Got> {
        let (channel, queue) = (self.channel(), queue.to_owned());
        self.run(async move {
            let message = ok(channel.basic_get(&queue, BasicGetOptions { no_ack: true }).await)?;
            let delivery = message.delivery;
            Some(Got {
                properties: delivery.properties,
                body: delivery.data,
                redelivered: delivery.redelivered,
            })
        })
    }

    /// Publish a message the way another writer would.
    fn plant(&self, queue: &str, body: &[u8]) {
        let (channel, queue, body) = (self.channel(), queue.to_owned(), body.to_vec());
        ok(self.run(async move {
            let options = BasicPublishOptions::default();
            let confirm =
                channel.basic_publish("", &queue, options, &body, BasicProperties::default());
            confirm.await?.await.map(|_| ())
        }));
    }

    /// Publish a message with `properties`, the way another writer would.
    fn plant_with(&self, queue: &str, body: &[u8], properties: BasicProperties) {
        let (channel, queue, body) = (self.channel(), queue.to_owned(), body.to_vec());
        ok(self.run(async move {
            let options = BasicPublishOptions::default();
            let confirm = channel.basic_publish("", &queue, options, &body, properties);
            confirm.await?.await.map(|_| ())
        }));
    }

    fn declare_exchange(&self, name: &str, bind_queue: &str, key: &str) {
        let (channel, name) = (self.channel(), name.to_owned());
        let (queue, key) = (bind_queue.to_owned(), key.to_owned());
        ok(self.run(async move {
            let declare = ExchangeDeclareOptions::default();
            channel
                .exchange_declare(&name, ExchangeKind::Direct, declare, FieldTable::default())
                .await?;
            channel
                .queue_bind(&queue, &name, &key, QueueBindOptions::default(), FieldTable::default())
                .await
        }));
    }

    fn delete(&self, queue: &str, exchange: Option<&str>) {
        let (channel, queue, exchange) =
            (self.channel(), queue.to_owned(), exchange.map(str::to_owned));
        self.run(async move {
            let _ = channel.queue_delete(&queue, QueueDeleteOptions::default()).await;
            if let Some(exchange) = exchange {
                let _ = channel.exchange_delete(&exchange, ExchangeDeleteOptions::default()).await;
            }
        });
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

/// Retry `check` until it holds (a close or a reject settles asynchronously).
fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One test's private queue (`topic`), deleted on drop together with its extra queues.
struct Ns {
    url: String,
    topic: String,
    raw: Raw,
}

impl Ns {
    fn new(test: &str) -> Option<Self> {
        let url = amqp_url();
        let Some(raw) = Raw::connect(&url) else {
            common::service_unavailable(test, &format!("no RabbitMQ at {url} (set XSM_AMQP_URL)"));
            return None;
        };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        Some(Self { url, topic: format!("xsmtest-{test}-{}-{nanos}", std::process::id()), raw })
    }

    fn broker(&self, options: &RabbitMqOptions) -> RabbitMqBroker {
        ok(RabbitMqBroker::open(&self.url, options))
    }

    fn options() -> RabbitMqOptions {
        RabbitMqOptions::new()
    }

    /// A second queue of this test.
    fn extra(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.topic)
    }

    fn ready(&self) -> Option<u32> {
        self.raw.count(&self.topic)
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        for suffix in ["", "a", "b"] {
            self.raw.delete(&self.extra(suffix), None);
        }
        self.raw.delete(&self.extra("-q"), Some(&self.extra("-ex")));
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
    let open = |o: &RabbitMqOptions| RabbitMqBroker::open(DEFAULT_URL, o);
    let bad = |o: RabbitMqOptions| matches!(open(&o), Err(EdaError::InvalidArgument(_)));
    assert!(bad(RabbitMqOptions::new().prefetch(0)));
    assert!(bad(RabbitMqOptions::new().batch(0)));
    assert!(bad(RabbitMqOptions::new().exchange("")));
    assert!(bad(RabbitMqOptions::new().io_timeout(Duration::ZERO)));
    let secret = "amqp://user:hunter2@host:notaport/";
    let Err(error) = RabbitMqBroker::open(secret, &RabbitMqOptions::new()) else {
        panic!("a bad URL must be refused")
    };
    assert!(matches!(error, EdaError::InvalidArgument(_)));
    assert!(!error.to_string().contains("hunter2"), "the URL is never echoed");
    assert!(!format!("{:?}", RabbitMqOptions::new()).contains("amqp"));
}

mod sync_api {
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    /// `n` deliveries of `topic`, each polled within three seconds.
    fn take(broker: &RabbitMqBroker, topic: &str, n: usize) -> Vec<Delivery> {
        (0..n)
            .map(|_| {
                ok(broker.poll(topic, Some(Duration::from_secs(3))))
                    .unwrap_or_else(|| panic!("a delivery in time"))
            })
            .collect()
    }

    fn idle(broker: &RabbitMqBroker, topic: &str) -> bool {
        ok(broker.poll(topic, Some(Duration::ZERO))).is_none()
    }

    #[test]
    fn an_unreachable_broker_fails_publish_and_marks_the_broker_unhealthy() {
        let broker = ok(RabbitMqBroker::open("amqp://127.0.0.1:1/%2f", &RabbitMqOptions::new()));
        assert!(broker.healthy());
        let result = broker.publish("orders", &envelope("s", 1));
        assert!(matches!(result, Err(BrokerError::Publish(_))), "{result:?}");
        assert!(!broker.healthy());
        assert!(matches!(broker.poll("orders", Some(Duration::ZERO)), Err(BrokerError::Other(_))));
    }

    #[test]
    fn publish_writes_the_python_queue_properties_and_headers() {
        let ns = ns!("layout");
        let broker = ns.broker(&Ns::options());
        let a = envelope("order:1", 1);
        let b = ok(Envelope::new("xsm.order.GO").build());
        ok(broker.publish(&ns.topic, &a));
        ok(broker.publish(&ns.topic, &b));

        let first = ns.raw.get(&ns.topic).unwrap_or_else(|| panic!("a message"));
        let p = &first.properties;
        assert_eq!(Got::text(p.content_type().as_ref()).as_deref(), Some(CE_CONTENT_TYPE));
        assert_eq!(Got::text(p.message_id().as_ref()).as_deref(), Some(a.id.as_str()));
        assert_eq!(Got::text(p.kind().as_ref()).as_deref(), Some("xsm.order.GO"));
        assert_eq!(*p.delivery_mode(), Some(2), "persistent");
        assert_eq!(first.header("ce_subject").as_deref(), Some("order:1"));
        assert_eq!(String::from_utf8_lossy(&first.body), ok(a.to_json(1 << 20)));
        assert!(!first.redelivered);
        let second = ns.raw.get(&ns.topic).unwrap_or_else(|| panic!("a message"));
        assert_eq!(
            second.header("ce_subject").as_deref(),
            Some(""),
            "no subject is the empty string"
        );
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
        assert_eq!((ns.ready(), broker.in_flight()), (Some(0), 1), "fetched, not yet settled");
        ok(broker.ack(&delivery));
        ok(broker.ack(&delivery));
        assert_eq!(broker.in_flight(), 0);
        // an acked message does not come back when the connection goes away
        ok(broker.close_blocking());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ns.ready(), Some(0));
        assert!(idle(&broker, &ns.topic), "close is followed by a reconnect");
    }

    #[test]
    fn an_idle_poll_waits_for_its_timeout_and_a_blocked_poll_sees_new_traffic() {
        let ns = ns!("idle");
        let broker = ns.broker(&Ns::options());
        let started = Instant::now();
        assert!(ok(broker.poll(&ns.topic, Some(Duration::from_millis(300)))).is_none());
        assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());

        let publisher = ns.broker(&Ns::options());
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
    fn nack_requeues_at_the_head_with_the_attempt_bumped_and_the_message_stays_unacked() {
        let ns = ns!("nack");
        let broker = ns.broker(&Ns::options());
        let (a, b) = (envelope("s", 1), envelope("s", 2));
        ok(broker.publish(&ns.topic, &a));
        ok(broker.publish(&ns.topic, &b));
        let mut first = take(&broker, &ns.topic, 1).remove(0);
        first.envelope.data = json!({"n": 99}); // a consumer mutating its copy changes nothing
        ok(broker.nack(&first, true));
        ok(broker.nack(&first, true)); // settling twice is a no-op
        assert_eq!(broker.held(&ns.topic), 2, "both fetched messages wait locally");

        let again = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((again.envelope.id.as_str(), again.envelope.attempt()), (a.id.as_str(), 1));
        assert_eq!(again.envelope.data, json!({"n": 1}));
        ok(broker.nack(&again, true));
        let third = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((third.envelope.id.as_str(), third.envelope.attempt()), (a.id.as_str(), 2));
        ok(broker.ack(&third));
        let second = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!((second.envelope.id.as_str(), second.envelope.attempt()), (b.id.as_str(), 0));

        // nack without requeue is basic.reject without requeue: never redelivered
        ok(broker.nack(&second, false));
        ok(broker.close_blocking());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ns.ready(), Some(0));
    }

    #[test]
    fn a_closed_connection_returns_unacked_messages_with_the_attempt_counted() {
        let ns = ns!("redeliver");
        let (doomed, rescuer) = (ns.broker(&Ns::options()), ns.broker(&Ns::options()));
        let sent = envelope("s", 1);
        ok(doomed.publish(&ns.topic, &sent));
        assert_eq!(take(&doomed, &ns.topic, 1)[0].envelope.attempt(), 0);
        assert!(idle(&rescuer, &ns.topic), "the delivery is held by the first consumer");
        ok(doomed.close_blocking()); // a crash: the broker requeues what was never acked
        let saved = take(&rescuer, &ns.topic, 1).remove(0);
        assert_eq!((saved.envelope.id.as_str(), saved.envelope.attempt()), (sent.id.as_str(), 1));
        ok(rescuer.ack(&saved));
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
            let n = delivery.envelope.data["n"].as_i64().unwrap_or(-1);
            per_subject.entry(subject).or_default().push(n);
            ok(broker.ack(&delivery));
        }
        assert_eq!(per_subject["one-subject"], (0..10).collect::<Vec<_>>());
        assert_eq!(per_subject["other"], (100..110).collect::<Vec<_>>());
    }

    #[test]
    fn consumers_of_one_queue_share_its_messages() {
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
    }

    #[test]
    fn prefetch_bounds_the_unacked_messages() {
        let ns = ns!("prefetch");
        let broker = ns.broker(&Ns::options().prefetch(2));
        for n in 0..5 {
            ok(broker.publish(&ns.topic, &envelope("s", n)));
        }
        let first = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(ns.ready(), Some(3), "only `prefetch` messages were fetched");
        let second = take(&broker, &ns.topic, 1).remove(0);
        assert!(idle(&broker, &ns.topic), "the window is full until something is acked");
        assert_eq!(ns.ready(), Some(3));
        ok(broker.ack(&first));
        let third = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(third.envelope.data["n"], 2);
        ok(broker.ack(&second));
        ok(broker.ack(&third));
        assert_eq!(take(&broker, &ns.topic, 2).len(), 2);
    }

    #[test]
    fn connect_properties_and_a_caller_managed_channel_are_supported() {
        let ns = ns!("custom");
        let connects = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&connects);
        let options = Ns::options().connect_with(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ConnectionProperties::default().with_connection_name("xsm-test".into())
        });
        let via_url = ns.broker(&options);
        ok(via_url.publish(&ns.topic, &envelope("s", 1)));
        ok(via_url.publish(&ns.topic, &envelope("s", 2)));
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 1, "one lazy connection");

        let managed = ok(RabbitMqBroker::with_channel(ns.raw.channel(), &Ns::options()));
        for delivery in take(&managed, &ns.topic, 2) {
            ok(managed.ack(&delivery));
        }
        ok(managed.close_blocking()); // the caller's channel is left open
        ok(managed.publish(&ns.topic, &envelope("s", 3)));
        assert_eq!(take(&managed, &ns.topic, 1)[0].envelope.data["n"], 3);
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
        assert_eq!(ns.ready(), None, "nothing was published, not even the queue");

        // messages other writers planted: oversized, not JSON; then a good one
        let good = envelope("s", 1);
        ok(broker.publish(&ns.topic, &good)); // declares the queue
        ns.raw.plant(&ns.topic, "x".repeat(1000).as_bytes());
        ns.raw.plant(&ns.topic, b"not json");
        let delivery = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(delivery.envelope, good);
        assert!(idle(&broker, &ns.topic));
        // the bad ones were rejected without requeue: only the unsettled good one returns
        ok(broker.close_blocking());
        eventually("only the good message is left", || ns.ready() == Some(1));
        let reports = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|(topic, _)| *topic == ns.topic));
        assert_eq!(reports[0].1, "EnvelopeTooLargeError");
    }

    #[test]
    fn an_exchange_routes_by_subject_and_a_missing_one_fails_then_recovers() {
        let ns = ns!("exchange");
        let (queue, exchange) = (ns.extra("-q"), ns.extra("-ex"));
        ns.raw.declare(&queue);
        ns.raw.declare_exchange(&exchange, &queue, "order:1");
        let broker = ns.broker(&Ns::options().exchange(exchange.clone()));
        let (routed, unrouted) = (envelope("order:1", 1), envelope("nobody", 2));
        ok(broker.publish("ignored-topic", &routed)); // the exchange, not the topic, routes
        // no binding: the broker returns the (mandatory) message instead of dropping it silently
        let refused = broker.publish("ignored-topic", &unrouted);
        assert!(matches!(refused, Err(BrokerError::Publish(_))), "{refused:?}");
        let got = take(&broker, &queue, 1).remove(0);
        assert_eq!(got.envelope, routed);
        assert!(idle(&broker, &queue));
        ok(broker.ack(&got));

        let missing = ns.broker(&Ns::options().exchange(ns.extra("-nope")));
        assert!(matches!(missing.publish("t", &routed), Err(BrokerError::Publish(_))));
        assert!(!missing.healthy());
        assert!(idle(&missing, &queue), "the next call reconnects");
        assert!(missing.healthy());
    }

    #[test]
    fn a_publish_nothing_accepts_fails_instead_of_vanishing() {
        let ns = ns!("unroutable");
        let (queue, exchange) = (ns.extra("-q"), ns.extra("-ex"));
        ns.raw.declare(&queue);
        ns.raw.declare_exchange(&exchange, &queue, "bound-subject");
        let broker = ns.broker(&Ns::options().exchange(exchange));
        let unbound = broker.publish("t", &envelope("not-bound", 1));
        let Err(BrokerError::Publish(why)) = unbound else {
            panic!("an unroutable message must fail the publish, got {unbound:?}")
        };
        assert!(why.contains("unroutable"), "{why}");
        // the bound subject still goes through, on the same channel
        ok(broker.publish("t", &envelope("bound-subject", 2)));
        eventually("the routed message is queued", || ns.raw.count(&queue) == Some(1));
        assert_eq!(ns.raw.count(&queue), Some(1), "only the routed one");
    }

    #[test]
    fn a_classic_queue_ignores_a_forged_delivery_count() {
        let ns = ns!("forged");
        ns.raw.declare(&ns.topic);
        let mut headers = FieldTable::default();
        headers.insert("x-delivery-count".into(), AMQPValue::LongLongInt(50));
        let body = ok(envelope("s", 1).to_json(1 << 20));
        ns.raw.plant_with(
            &ns.topic,
            body.as_bytes(),
            BasicProperties::default().with_headers(headers),
        );
        let broker = ns.broker(&Ns::options());
        let got = take(&broker, &ns.topic, 1).remove(0);
        assert_eq!(got.envelope.attempt(), 0, "a publisher cannot set the attempt");
    }

    #[test]
    fn quorum_queues_are_declared_on_request_and_count_redeliveries() {
        let ns = ns!("quorum");
        // the type is part of the declaration: a queue that exists as classic refuses it
        ns.raw.declare(&ns.topic);
        let quorum = Ns::options().quorum_queues(true);
        let clash = ns.broker(&quorum).publish(&ns.topic, &envelope("s", 1));
        assert!(matches!(clash, Err(BrokerError::Publish(_))), "{clash:?}");

        let fresh = ns.extra("a");
        let broker = ns.broker(&quorum);
        ok(broker.publish(&fresh, &envelope("s", 2)));
        let first = take(&broker, &fresh, 1).remove(0);
        assert_eq!(first.envelope.attempt(), 0);
        // an un-acked message goes back when the connection closes, with the broker's own count
        ok(broker.close_blocking());
        let again = ns.broker(&quorum);
        let redelivered = take(&again, &fresh, 1).remove(0);
        assert_eq!(redelivered.envelope.attempt(), 1);
    }

    #[test]
    fn a_broker_that_never_answers_times_out() {
        let port = common::silent_server();
        let options = Ns::options().io_timeout(Duration::from_millis(300));
        let broker = ok(RabbitMqBroker::open(&format!("amqp://127.0.0.1:{port}/%2f"), &options));
        let started = Instant::now();
        let result = broker.publish("orders", &envelope("s", 1));
        let Err(BrokerError::Publish(why)) = result else {
            panic!("expected a timeout: {result:?}")
        };
        assert!(why.contains("timed out"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(10), "bounded by io_timeout");
        assert!(!broker.healthy());
        let polled = broker.poll("orders", Some(Duration::ZERO));
        assert!(matches!(polled, Err(BrokerError::Other(ref why)) if why.contains("timed out")));
    }
}

mod async_api {
    use xstate_eda::BrokerAdapter;

    use super::*;

    async fn poll3(broker: &RabbitMqBroker, topic: &str) -> Result<Option<Delivery>, BrokerError> {
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
        assert!(ok(BrokerAdapter::poll(&broker, &ns.topic, Some(Duration::ZERO)).await).is_none());

        // settled through the other trait: one delivery handle works with both
        ok(BrokerAdapter::publish(&broker, &ns.topic, &envelope("s", 2)).await);
        let third = ok(poll3(&broker, &ns.topic).await).unwrap_or_else(|| panic!("delivered"));
        ok(BrokerAdapter::nack(&broker, &third, false).await);
        ok(broker.close().await);
        let queue = ns.topic.clone();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ns.raw.count(&queue), Some(0), "acked and rejected messages stay gone");
    }

    #[tokio::test]
    async fn a_poll_dropped_mid_wait_loses_nothing() {
        let ns = ns!("cancel");
        let broker = ns.broker(&Ns::options());
        // cancelled while waiting for traffic ...
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
}

#[path = "rabbitmq/cross_language.rs"]
mod cross_language;
