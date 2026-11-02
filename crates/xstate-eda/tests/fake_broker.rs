#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The in-memory brokers: FIFO per topic, nack redelivery at the head with `attempt + 1`,
//! failure injection, the wire size cap and `drain()`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use xstate_eda::{
    BrokerAdapter, BrokerError, EdaError, Envelope, EnvelopeError, FakeBrokerAdapter,
    SyncBrokerAdapter, SyncFakeBrokerAdapter,
};

fn env(subject: &str, n: i64) -> Envelope {
    Envelope::new("xsm.o.GO").subject(subject).data(json!({"n": n})).build().unwrap()
}

#[test]
fn nack_requeues_at_the_head_with_the_attempt_bumped() {
    let broker = SyncFakeBrokerAdapter::new();
    let (a, b) = (env("s", 1), env("s", 2));
    broker.publish("t", &a).unwrap();
    broker.publish("t", &b).unwrap();
    let mut first = broker.poll("t", Some(Duration::ZERO)).unwrap().unwrap();
    assert_eq!((first.envelope.id.as_str(), first.envelope.attempt()), (a.id.as_str(), 0));
    first.envelope.data = json!({"n": 99}); // a consumer mutating its copy changes nothing
    broker.nack(&first, true).unwrap();
    broker.nack(&first, true).unwrap(); // settling twice is a no-op
    assert_eq!(broker.pending("t"), 2);
    let again = broker.poll("t", Some(Duration::ZERO)).unwrap().unwrap();
    assert_eq!(again.envelope.id, a.id);
    assert_eq!(again.envelope.attempt(), 1);
    assert_eq!(again.envelope.data, json!({"n": 1}));
    broker.ack(&again).unwrap();
    let dropped = broker.poll("t", Some(Duration::ZERO)).unwrap().unwrap();
    broker.nack(&dropped, false).unwrap();
    assert_eq!(broker.pending("t"), 0);
    assert_eq!(broker.acked().len(), 1);
    assert_eq!(broker.nacked().len(), 2);
    assert_eq!(broker.published().len(), 2);
    assert_eq!(broker.published_on("t")[1].id, b.id);
    assert_eq!(broker.topics(), ["t"]);
    assert_eq!(broker.in_flight(), 0);
}

#[test]
fn deliveries_of_another_broker_or_after_clear_are_unknown() {
    let (one, two) = (SyncFakeBrokerAdapter::new(), SyncFakeBrokerAdapter::new());
    one.deliver("t", &env("s", 1)).unwrap();
    let d = one.poll("t", Some(Duration::ZERO)).unwrap().unwrap();
    two.ack(&d).unwrap();
    assert_eq!(one.in_flight(), 1);
    one.clear();
    one.ack(&d).unwrap();
    assert_eq!(one.acked().len(), 0);
    assert_eq!(one.in_flight(), 0);
}

#[test]
fn injected_failures_come_first_and_in_order() {
    let broker = SyncFakeBrokerAdapter::with_max_bytes(200);
    assert!(matches!(broker.fail_next_publish(None, 0), Err(EdaError::InvalidArgument(_))));
    broker.fail_next_publish(None, 1).unwrap();
    broker.fail_next_publish(Some(BrokerError::Other("down".into())), 1).unwrap();
    let big = Envelope::new("t").data(json!({"x": "y".repeat(500)})).build().unwrap();
    assert_eq!(broker.publish("t", &big), Err(BrokerError::Publish("injected".into())));
    assert_eq!(broker.publish("t", &big), Err(BrokerError::Other("down".into())));
    let err = broker.publish("t", &big).unwrap_err();
    assert!(matches!(err, BrokerError::Envelope(EnvelopeError::TooLarge(_))), "{err:?}");
    assert!(matches!(
        broker.deliver("t", &big),
        Err(BrokerError::Envelope(EnvelopeError::TooLarge(_)))
    ));
    assert_eq!(broker.published().len(), 0);
}

#[test]
fn subscribe_stops_after_idle_time_not_mid_backlog() {
    let broker = SyncFakeBrokerAdapter::new();
    for n in 0..5 {
        broker.deliver("t", &env("s", n)).unwrap();
    }
    let got: Vec<_> = broker.subscribe("t", Some(Duration::ZERO)).map(Result::unwrap).collect();
    assert_eq!(got.len(), 5);
    assert_eq!(broker.in_flight(), 5);
    let start = Instant::now();
    assert!(broker.poll("t", Some(Duration::from_millis(30))).unwrap().is_none());
    assert!(start.elapsed() >= Duration::from_millis(30));
}

#[test]
fn drain_runs_handlers_and_their_output() {
    let broker = Arc::new(SyncFakeBrokerAdapter::new());
    let relay = Arc::clone(&broker);
    broker.on("in", move |e| {
        let out = Envelope::new("out").subject(e.subject.clone().unwrap()).build().unwrap();
        relay.publish("out", &out).map_err(|e| e.to_string())
    });
    broker.on("out", |_| Ok(()));
    for n in 0..3 {
        broker.deliver("in", &env("s", n)).unwrap();
    }
    assert_eq!(broker.drain(None, 100).unwrap(), 6);
    assert_eq!(broker.acked().len(), 6);
    assert!(matches!(broker.drain(None, 0), Err(EdaError::InvalidArgument(_))));
}

#[test]
fn drain_failures_nack_without_requeue_and_the_limit_is_exact() {
    let broker = SyncFakeBrokerAdapter::new();
    broker.on("t", |e| if e.data["n"] == 1 { Err("bad".into()) } else { Ok(()) });
    for n in 0..3 {
        broker.deliver("t", &env("s", n)).unwrap();
    }
    let err = broker.drain(Some("t"), 10).unwrap_err();
    assert_eq!(err.to_string(), "handler failed: bad");
    assert_eq!(broker.nacked().len(), 1);
    assert_eq!(broker.pending("t"), 1);
    assert_eq!(broker.drain(Some("t"), 1).unwrap(), 1);

    let looping = Arc::new(SyncFakeBrokerAdapter::new());
    let again = Arc::clone(&looping);
    looping.on("t", move |e| again.publish("t", e).map_err(|e| e.to_string()));
    looping.deliver("t", &env("s", 0)).unwrap();
    assert!(matches!(looping.drain(None, 50), Err(EdaError::DrainLimit { handled: 50 })));
}

#[test]
fn a_panicking_drain_handler_releases_its_delivery() {
    let broker = SyncFakeBrokerAdapter::new();
    broker.on("t", |_| panic!("test bug"));
    broker.deliver("t", &env("s", 0)).unwrap();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| broker.drain(None, 10)));
    assert!(caught.is_err());
    assert_eq!(broker.pending("t"), 1);
    assert_eq!(broker.nacked().len(), 0);
    let d = broker.poll("t", Some(Duration::ZERO)).unwrap().unwrap();
    assert_eq!(d.envelope.attempt(), 0);
}

#[test]
fn async_poll_waits_for_traffic_without_a_runtime() {
    let broker = Arc::new(FakeBrokerAdapter::new());
    let producer = Arc::clone(&broker);
    let e = env("s", 1);
    let sent = e.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        common::block_on(producer.publish("t", &sent)).unwrap();
    });
    let d = common::block_on(broker.poll("t", Some(Duration::from_secs(5)))).unwrap().unwrap();
    t.join().unwrap();
    assert_eq!(d.envelope.id, e.id);
    common::block_on(broker.nack(&d, true)).unwrap();
    let d = common::block_on(broker.poll("t", Some(Duration::ZERO))).unwrap().unwrap();
    assert_eq!(d.envelope.attempt(), 1);
    common::block_on(broker.ack(&d)).unwrap();
    assert!(common::block_on(broker.poll("t", Some(Duration::from_millis(20)))).unwrap().is_none());
}
