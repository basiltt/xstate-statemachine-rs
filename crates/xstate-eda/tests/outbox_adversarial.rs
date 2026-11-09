#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Adversarial outbox coverage: concurrent relays on a shared store, crash between claim and
//! publish, and `flush_into` putting un-added rows back when the store fails.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic, Observer, SyncInterpreter};
use xstate_eda::{
    EdaError, Envelope, MemoryOutboxStore, OutboxPlugin, OutboxRecord, OutboxRelay, OutboxStore,
    SyncBrokerAdapter, SyncFakeBrokerAdapter, WallClock,
};

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

#[cfg(feature = "sqlite")]
#[test]
fn two_concurrent_relays_drain_a_shared_sqlite_outbox_without_duplicates() {
    use xstate_eda::SqliteOutboxStore;

    let store: Arc<dyn OutboxStore> = Arc::new(SqliteOutboxStore::open_in_memory().unwrap());
    for n in 0..20 {
        store.add("orders", &env(n)).unwrap();
    }
    let a = OutboxRelay::new(Arc::clone(&store), SyncFakeBrokerAdapter::new()).batch(100).unwrap();
    let b = OutboxRelay::new(Arc::clone(&store), SyncFakeBrokerAdapter::new()).batch(100).unwrap();
    let ta = std::thread::spawn(move || a.relay_once_sync());
    let tb = std::thread::spawn(move || b.relay_once_sync());
    let na = ta.join().unwrap().unwrap();
    let nb = tb.join().unwrap().unwrap();
    // the lease partitions the rows: no row is published by both relays
    assert_eq!(na + nb, 20, "relay a took {na}, relay b took {nb}");
    assert_eq!(store.pending(100).unwrap().len(), 0, "everything went out");
}

#[cfg(feature = "sqlite")]
#[test]
fn a_relay_that_crashes_between_claim_and_publish_loses_its_lease_and_the_row_is_reclaimed() {
    use xstate_eda::SqliteOutboxStore;

    let clock = FakeClock::new(1_000.0);
    let store: Arc<dyn OutboxStore> =
        Arc::new(SqliteOutboxStore::open_in_memory().unwrap().with_clock(clock.wall()));
    store.add("orders", &env(1)).unwrap();
    store.add("orders", &env(2)).unwrap();

    // relay A performs its claim step but "crashes" before publishing: it never marks sent nor
    // releases, exactly the failure `relay_once` cannot recover from itself
    let claimed = store.claim(10, "relay-a", 10.0).unwrap().unwrap();
    assert_eq!(claimed.len(), 2);

    // relay B, still within the lease, gets nothing
    let b = OutboxRelay::new(Arc::clone(&store), SyncFakeBrokerAdapter::new())
        .owner("relay-b")
        .lease_s(10.0)
        .unwrap();
    assert_eq!(b.relay_once_sync().unwrap(), 0);

    // the lease runs out: B now takes the rows over and publishes them
    clock.set(1_011.0);
    assert_eq!(b.relay_once_sync().unwrap(), 2);
    assert_eq!(store.pending(100).unwrap().len(), 0);
}

/// A store wrapper that fails its `n`-th `add`.
struct FailAdds {
    inner: MemoryOutboxStore,
    calls: AtomicU64,
    fail_at: u64,
}

impl OutboxStore for FailAdds {
    fn add(&self, topic: &str, envelope: &Envelope) -> xstate_eda::Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_at {
            return Err(EdaError::Store(xstate_persistence::StoreError::Unavailable(
                "add down".into(),
            )));
        }
        self.inner.add(topic, envelope)
    }
    fn pending(&self, limit: usize) -> xstate_eda::Result<Vec<OutboxRecord>> {
        self.inner.pending(limit)
    }
    fn mark_sent(&self, seqs: &[i64]) -> xstate_eda::Result<usize> {
        self.inner.mark_sent(seqs)
    }
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
fn flush_into_puts_un_added_rows_back_when_the_store_fails_midway() {
    let plugin = Arc::new(OutboxPlugin::new().topic("m.events"));
    let mut interp = SyncInterpreter::new(three_states(), MachineLogic::dynamic());
    interp.observe(plugin.clone() as Arc<dyn Observer<Value>>);
    interp.start().unwrap();
    interp.send("GO").unwrap(); // buffers "a" (initial) + "b"
    interp.send("GO").unwrap(); // buffers "c"
    assert_eq!(plugin.buffered(), 3);

    let store = FailAdds { inner: MemoryOutboxStore::new(), calls: AtomicU64::new(0), fail_at: 2 };
    let err = plugin.flush_into(&store).unwrap_err();
    assert!(matches!(err, EdaError::Store(_)), "{err:?}");
    // the first row reached the store; the second and third are back in the buffer
    assert_eq!(store.inner.pending(10).unwrap().len(), 1);
    assert_eq!(plugin.buffered(), 2);
    let kept: Vec<String> = plugin.drain().into_iter().map(|e| e.ty).collect();
    assert_eq!(kept, ["xsm.m.transition.b", "xsm.m.transition.c"]);
}

#[test]
fn a_store_without_leases_falls_back_to_pending() {
    // a store whose claim/release return None is drained through pending() (single relay)
    struct NoLeases(MemoryOutboxStore);
    impl OutboxStore for NoLeases {
        fn add(&self, t: &str, e: &Envelope) -> xstate_eda::Result<()> {
            self.0.add(t, e)
        }
        fn pending(&self, limit: usize) -> xstate_eda::Result<Vec<OutboxRecord>> {
            self.0.pending(limit)
        }
        fn mark_sent(&self, seqs: &[i64]) -> xstate_eda::Result<usize> {
            self.0.mark_sent(seqs)
        }
    }
    let store = NoLeases(MemoryOutboxStore::new());
    store.add("orders", &env(1)).unwrap();
    store.add("orders", &env(2)).unwrap();
    let relay = OutboxRelay::new(Arc::new(store), SyncFakeBrokerAdapter::new());
    assert_eq!(relay.relay_once_sync().unwrap(), 2);
}

#[test]
fn a_relay_lease_of_nan_or_zero_is_refused() {
    for bad in [f64::NAN, 0.0, -1.0] {
        let store: Arc<dyn OutboxStore> = Arc::new(MemoryOutboxStore::new());
        let relay = OutboxRelay::new(store, SyncFakeBrokerAdapter::new());
        assert!(matches!(relay.lease_s(bad), Err(EdaError::InvalidArgument(_))), "{bad}");
    }
    let store: Arc<dyn OutboxStore> = Arc::new(MemoryOutboxStore::new());
    let relay = OutboxRelay::new(store, SyncFakeBrokerAdapter::new()).lease_s(1.0).unwrap();
    let _: &dyn SyncBrokerAdapter = relay.broker();
    let _guard = Mutex::new(0);
}
