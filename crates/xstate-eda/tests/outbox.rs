#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(not(feature = "sqlite"), allow(missing_docs))] // the whole file needs SQLite
#![cfg(feature = "sqlite")]
//! The outbox: stores with claim leases, relay crash recovery and failure handling, the
//! `OutboxPlugin` driven by a real interpreter, and `persisted()` integration.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use xstate_core::{Event, Machine, MachineLogic, Observer, SyncInterpreter};
use xstate_eda::{
    BrokerError, EdaError, Envelope, FakeBrokerAdapter, MemoryOutboxStore, OutboxPlugin,
    OutboxRelay, OutboxStore, SqliteOutboxStore, SyncBrokerAdapter, SyncFakeBrokerAdapter,
    WallClock,
};
use xstate_persistence::{MemoryStore, PersistedOptions, persisted};

/// A settable wall clock.
#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    fn new(at: f64) -> Self {
        Self(Arc::new(AtomicU64::new(at.to_bits())))
    }
    fn set(&self, at: f64) {
        self.0.store(at.to_bits(), Ordering::SeqCst);
    }
    fn wall(&self) -> WallClock {
        let inner = Arc::clone(&self.0);
        Arc::new(move || f64::from_bits(inner.load(Ordering::SeqCst)))
    }
}

fn env(n: i64) -> Envelope {
    Envelope::new("order.paid").subject(format!("o-{n}")).data(json!({"n": n})).build().unwrap()
}

fn stores(clock: &FakeClock) -> Vec<Arc<dyn OutboxStore>> {
    vec![
        Arc::new(MemoryOutboxStore::new().with_clock(clock.wall())),
        Arc::new(SqliteOutboxStore::open_in_memory().unwrap().with_clock(clock.wall())),
    ]
}

#[test]
fn leases_partition_rows_and_expire_for_crash_recovery() {
    let clock = FakeClock::new(1000.0);
    for store in stores(&clock) {
        clock.set(1000.0);
        for n in 0..4 {
            store.add("orders", &env(n)).unwrap();
        }
        let a = store.claim(2, "relay-a", 30.0).unwrap().unwrap();
        assert_eq!(a.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
        let b = store.claim(10, "relay-b", 30.0).unwrap().unwrap();
        assert_eq!(b.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4], "a's rows are leased");
        // relay-a re-claims its own rows; relay-a "crashes" and its lease runs out
        assert_eq!(store.claim(10, "relay-a", 30.0).unwrap().unwrap().len(), 2);
        clock.set(1031.0);
        let c = store.claim(10, "relay-c", 30.0).unwrap().unwrap();
        assert_eq!(c.len(), 4, "every expired lease is taken over");
        assert_eq!(store.release(&[1, 2, 99], "relay-c").unwrap(), Some(2));
        assert_eq!(store.release(&[3], "someone-else").unwrap(), Some(0));
        assert_eq!(store.claim(10, "relay-d", 30.0).unwrap().unwrap().len(), 2);
        assert_eq!(store.mark_sent(&[1, 2, 1]).unwrap(), 2);
        assert_eq!(store.pending(10).unwrap().len(), 2);
    }
}

#[test]
fn relay_publishes_in_order_and_marks_sent() {
    let clock = FakeClock::new(1.0);
    for store in stores(&clock) {
        for n in 0..3 {
            store.add("orders", &env(n)).unwrap();
        }
        let relay =
            OutboxRelay::new(Arc::clone(&store), SyncFakeBrokerAdapter::new()).batch(2).unwrap();
        assert_eq!(relay.relay_once_sync().unwrap(), 2);
        assert_eq!(relay.relay_once_sync().unwrap(), 1);
        assert_eq!(relay.relay_once_sync().unwrap(), 0);
        let subjects: Vec<_> =
            relay.broker().published_on("orders").into_iter().map(|e| e.subject.unwrap()).collect();
        assert_eq!(subjects, ["o-0", "o-1", "o-2"]);
        assert_eq!(store.pending(10).unwrap().len(), 0);
    }
}

#[test]
fn a_failing_broker_marks_what_it_took_and_releases_the_rest() {
    let clock = FakeClock::new(1.0);
    for store in stores(&clock) {
        for n in 0..3 {
            store.add("orders", &env(n)).unwrap();
        }
        let broker = SyncFakeBrokerAdapter::new();
        broker.publish("warmup", &env(9)).unwrap();
        let relay = OutboxRelay::new(Arc::clone(&store), broker).owner("a");
        // the second publish of the batch fails
        relay.broker().fail_next_publish(None, 1).unwrap();
        let err = relay.relay_once_sync().unwrap_err();
        assert!(matches!(err, EdaError::Broker(BrokerError::Publish(_))), "{err:?}");
        assert_eq!(store.pending(10).unwrap().len(), 3, "nothing reached the broker");
        // released at once: another relay need not wait out the lease
        let other = OutboxRelay::new(Arc::clone(&store), SyncFakeBrokerAdapter::new()).owner("b");
        assert_eq!(other.relay_once_sync().unwrap(), 3);
    }
}

#[test]
fn partial_batches_are_settled_row_by_row() {
    // a broker refusing the second envelope: the first is marked sent, two stay pending
    struct Flaky(SyncFakeBrokerAdapter, Mutex<usize>);
    impl SyncBrokerAdapter for Flaky {
        fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
            let mut n = self.1.lock().unwrap();
            *n += 1;
            if *n == 2 {
                return Err(BrokerError::Publish("down".into()));
            }
            self.0.publish(topic, envelope)
        }
        fn poll(
            &self,
            t: &str,
            d: Option<std::time::Duration>,
        ) -> Result<Option<xstate_eda::Delivery>, BrokerError> {
            self.0.poll(t, d)
        }
        fn ack(&self, d: &xstate_eda::Delivery) -> Result<(), BrokerError> {
            self.0.ack(d)
        }
        fn nack(&self, d: &xstate_eda::Delivery, r: bool) -> Result<(), BrokerError> {
            self.0.nack(d, r)
        }
    }
    let store: Arc<dyn OutboxStore> = Arc::new(MemoryOutboxStore::new());
    for n in 0..3 {
        store.add("orders", &env(n)).unwrap();
    }
    let relay =
        OutboxRelay::new(Arc::clone(&store), Flaky(SyncFakeBrokerAdapter::new(), Mutex::new(0)));
    assert!(relay.relay_once_sync().is_err());
    assert_eq!(store.pending(10).unwrap().iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(relay.relay_once_sync().unwrap(), 2);
    assert!(OutboxRelay::new(store, SyncFakeBrokerAdapter::new()).lease_s(0.0).is_err());
}

#[test]
fn async_relay_drains_into_an_async_broker() {
    let store: Arc<dyn OutboxStore> = Arc::new(SqliteOutboxStore::open_in_memory().unwrap());
    for n in 0..5 {
        store.add("orders", &env(n)).unwrap();
    }
    let relay = OutboxRelay::new(Arc::clone(&store), FakeBrokerAdapter::new());
    assert_eq!(common::block_on(relay.relay_once()).unwrap(), 5);
    assert_eq!(relay.broker().published().len(), 5);
}

#[test]
fn sqlite_outbox_upgrades_old_tables_and_purges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE xsm_outbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL, \
             topic TEXT NOT NULL, subject TEXT, envelope TEXT NOT NULL, created_at REAL NOT NULL, \
             sent_at REAL)",
        )
        .unwrap();
    }
    let clock = FakeClock::new(100.0);
    let store = SqliteOutboxStore::open(&path).unwrap().with_clock(clock.wall());
    store.add("t", &env(1)).unwrap();
    assert_eq!(store.claim(5, "r", 10.0).unwrap().unwrap().len(), 1);
    store.mark_sent(&[1]).unwrap();
    clock.set(100.0 + 86_401.0);
    assert_eq!(store.purge_sent(86_400.0).unwrap(), 1);
    assert_eq!(store.count(false).unwrap(), 0);
}

fn order() -> Machine {
    Machine::from_value(&common::golden("asyncapi.json")[0]["config"]).unwrap()
}

#[test]
fn plugin_publishes_what_the_chart_declares() {
    let plugin = Arc::new(OutboxPlugin::new().instance_key("order-7"));
    let mut interp = SyncInterpreter::new(order(), MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    assert_eq!(plugin.buffered(), 0, "the initial state is not tagged");

    interp.send("NOTE").unwrap();
    let note = plugin.drain();
    assert_eq!(note.len(), 1);
    assert_eq!(note[0].ty, "xsm.order.transition.pending");
    assert_eq!(note[0].subject.as_deref(), Some("order-7"));
    assert_eq!(note[0].source, "xsm/order");
    assert_eq!(note[0].machineversion.as_deref(), Some("1.2.0"));
    assert_eq!(note[0].correlationid.as_deref(), Some(note[0].id.as_str()));

    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let cause = Envelope::new("xsm.order.PAY")
        .subject("biz-1")
        .correlationid("corr-1")
        .extension("traceparent", tp)
        .build()
        .unwrap();
    plugin.set_cause(Some(cause.clone()));
    interp.send(Event::with_payload("PAY", json!({}))).unwrap();
    let paid = plugin.drain();
    let types: Vec<&str> = paid.iter().map(|e| e.ty.as_str()).collect();
    assert_eq!(types, ["order.paid", "xsm.order.transition.paid"]);
    assert_eq!(paid[0].data, json!({"total": 0, "customer": "c-1"}));
    assert_eq!(paid[1].data, json!({"total": 0, "customer": "c-1", "card_token": "***"}));
    for e in &paid {
        assert_eq!(e.subject.as_deref(), Some("biz-1"));
        assert_eq!(e.causationid.as_deref(), Some(cause.id.as_str()));
        assert_eq!(e.correlationid.as_deref(), Some("corr-1"));
        assert_eq!(e.extensions["traceparent"], tp);
    }
    plugin.set_cause(None);
    interp.send("SHIP").unwrap();
    assert_eq!(plugin.drain()[0].ty, "xsm.order.transition.shipped");
}

#[test]
fn plugin_publishes_tagged_initial_states_and_flushes_into_a_store() {
    let machine = Machine::from_value(&json!({
        "id": "m", "initial": "a", "context": {"password": "p", "n": 1},
        "states": {"a": {"tags": ["publish"], "on": {"GO": "b"}}, "b": {"tags": "publish"}}
    }))
    .unwrap();
    let plugin = Arc::new(OutboxPlugin::new().topic("m.events"));
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GO").unwrap();
    let store = MemoryOutboxStore::new();
    assert_eq!(plugin.flush_into(&store).unwrap(), 2);
    let rows = store.pending(10).unwrap();
    assert_eq!(rows[0].topic, "m.events");
    assert_eq!(rows[0].envelope.ty, "xsm.m.transition.a");
    assert_eq!(rows[0].envelope.data, json!({"password": "***", "n": 1}));
    assert_eq!(rows[1].envelope.ty, "xsm.m.transition.b");
    assert_eq!(plugin.discard(), 0);
}

#[test]
fn plugin_inside_persisted_publishes_after_commit() {
    let store = MemoryStore::new();
    let broker = Arc::new(SyncFakeBrokerAdapter::new());
    let plugin = Arc::new(OutboxPlugin::new().instance_key("o-1"));
    let options = PersistedOptions::<Value> {
        observers: vec![plugin.clone() as Arc<dyn Observer<Value>>],
        ..PersistedOptions::default()
    };
    let machine = order();
    persisted(&store, "o-1", &machine, &MachineLogic::dynamic(), &options, |interp, session| {
        interp.send("PAY")?;
        let batch = plugin.drain();
        let broker = Arc::clone(&broker);
        session.after_commit(move || {
            for e in &batch {
                broker
                    .publish("events", e)
                    .map_err(|e| xstate_persistence::StoreError::Other(e.to_string()))?;
            }
            Ok(())
        });
        Ok(())
    })
    .unwrap();
    assert_eq!(broker.published_on("events").len(), 2);
    // a failing body never publishes
    let res = persisted(
        &store,
        "o-1",
        &machine,
        &MachineLogic::dynamic(),
        &options,
        |interp, session| {
            interp.send("SHIP")?;
            let batch = plugin.drain();
            let broker = Arc::clone(&broker);
            session.after_commit(move || {
                for e in &batch {
                    let _ = broker.publish("events", e);
                }
                Ok(())
            });
            Err::<(), _>(xstate_persistence::StoreError::Other("abort".into()))
        },
    );
    assert!(res.is_err());
    assert_eq!(broker.published_on("events").len(), 2);
}

#[test]
fn a_zero_relay_batch_is_refused() {
    let store: Arc<dyn OutboxStore> = Arc::new(MemoryOutboxStore::new());
    let relay = OutboxRelay::new(store, SyncFakeBrokerAdapter::new());
    assert!(matches!(relay.batch(0), Err(EdaError::InvalidArgument(_))));
}

fn three_states() -> Machine {
    Machine::from_value(&json!({
        "id": "m", "initial": "a", "context": {"n": 1},
        "states": {
            "a": {"tags": ["publish"], "on": {"GO": "b"}},
            "b": {"tags": ["publish"], "on": {"GO": "c"}},
            "c": {"tags": ["publish"]},
        }
    }))
    .unwrap()
}

#[test]
fn the_plugin_buffer_is_bounded_and_counts_what_it_drops() {
    assert!(matches!(OutboxPlugin::new().max_buffered(0), Err(EdaError::InvalidArgument(_))));
    let plugin = Arc::new(OutboxPlugin::new().max_buffered(2).unwrap());
    let mut interp = SyncInterpreter::new(three_states(), MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GO").unwrap();
    interp.send("GO").unwrap();
    assert_eq!(plugin.buffered(), 2);
    assert_eq!(plugin.dropped(), 1, "the third envelope did not fit");
    let kept: Vec<String> = plugin.drain().into_iter().map(|e| e.ty).collect();
    assert_eq!(kept, ["xsm.m.transition.a", "xsm.m.transition.b"], "the oldest are kept");
    // draining makes room again
    assert_eq!(plugin.dropped(), 1);
}

#[test]
fn a_malformed_publish_spec_is_reported_not_swallowed() {
    let machine = Machine::from_value(&json!({
        "id": "m", "initial": "a", "context": {},
        "states": {
            "a": {"on": {"GO": {"target": "b", "meta": {"publish": 5}}}},
            "b": {},
        }
    }))
    .unwrap();
    let plugin = Arc::new(OutboxPlugin::new());
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GO").unwrap();
    let failures = plugin.failures();
    assert_eq!(failures.len(), 1, "reported once, not once per step: {failures:?}");
    assert!(failures[0].reason.contains("meta.publish"), "{failures:?}");
    assert!(matches!(plugin.try_drain(), Err(EdaError::InvalidSpec(_))));
    assert!(plugin.drain().is_empty(), "and nothing was published in its place");
    assert_eq!(plugin.take_failures().len(), 1);
    assert_eq!(plugin.failures().len(), 0);
}

#[test]
fn a_clean_chart_drains_without_failures() {
    let plugin = Arc::new(OutboxPlugin::new());
    let mut interp = SyncInterpreter::new(three_states(), MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    assert_eq!(plugin.try_drain().unwrap().len(), 1);
    assert_eq!(plugin.failures().len(), 0);
}

#[test]
fn a_corrupt_sqlite_row_is_parked_and_later_rows_still_relay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poison.sqlite");
    let store = Arc::new(SqliteOutboxStore::open(&path).unwrap());
    store.add("orders", &env(1)).unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO xsm_outbox(id, topic, envelope, created_at) VALUES ('x', 'orders', 'not json', 1.0)",
            [],
        )
        .unwrap();
    }
    store.add("orders", &env(3)).unwrap();
    // the read path skips the corrupt row without parking it
    assert_eq!(store.pending(10).unwrap().len(), 2);
    let relay =
        OutboxRelay::new(store.clone() as Arc<dyn OutboxStore>, SyncFakeBrokerAdapter::new());
    assert_eq!(relay.relay_once_sync().unwrap(), 2, "the rows around the corrupt one went out");
    assert_eq!(relay.broker().published_on("orders").len(), 2);
    // the corrupt row stays unsent, parked under the poison lease: nobody claims it again
    assert_eq!(store.count(true).unwrap(), 1);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let owner: String = conn
        .query_row("SELECT claimed_by FROM xsm_outbox WHERE sent_at IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(owner, xstate_eda::POISON_OWNER);
    let claimed = store.claim(10, "other", 30.0).unwrap().unwrap();
    assert_eq!(claimed, Vec::new(), "parked rows are not claimable");
}

#[test]
fn a_publish_field_missing_from_the_context_is_published_as_null() {
    let machine = Machine::from_value(&json!({
        "id": "m", "initial": "a", "context": {"total": 5},
        "states": {
            "a": {"on": {"GO": {"target": "b",
                "meta": {"publish": {"type": "m.went", "data": ["total", "totl"]}}}}},
            "b": {},
        }
    }))
    .unwrap();
    let plugin = Arc::new(OutboxPlugin::new());
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GO").unwrap();
    let sent = plugin.try_drain().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].data, json!({"total": 5, "totl": null}));
    assert!(plugin.failures().is_empty(), "a warning, not a failure");
}
