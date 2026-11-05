#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `DueTimerScanner`: wake persisted `after` deadlines.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{stored_snapshot, version_of};
use serde_json::{Value, json};
use xstate_core::snapshot::Deadline;
use xstate_core::{Clock, Machine, MachineLogic, SimulatedClock};
use xstate_persistence::locking::{
    LockStrategy, NoLock, OptimisticLock, PersistedOptions, PessimisticLock, persisted,
};
use xstate_persistence::memory_store::MemoryStore;
use xstate_persistence::timers::{
    DeadlineExt, DueTimerScanner, FixedWallClock, MAX_REMAINING_MS, MachineFactory, check_record,
};
use xstate_persistence::{Health, LockGuard, SaveOptions, StateStore, StoreError, StoredSnapshot};

/// Wall time (epoch seconds) at which the fixtures are written; deadlines land at `T0 + 1`.
const T0: f64 = 1000.0;

fn timer_machine() -> Machine {
    Machine::from_value(&json!({
        "id": "timer", "initial": "waiting",
        "states": { "waiting": { "after": { "1000": "expired" } }, "expired": {} }
    }))
    .unwrap()
}

/// Same chart, but the timer transition is guarded by `g`, which runs `on_guard` inside the
/// restore (after the scanner loaded, before it saves).
fn guarded_timer_machine() -> Machine {
    Machine::from_value(&json!({
        "id": "timer", "initial": "waiting",
        "states": {
            "waiting": { "after": { "1000": { "target": "expired", "guard": "g" } } },
            "expired": {}
        }
    }))
    .unwrap()
}

fn guard_logic(on_guard: impl Fn() + Send + Sync + 'static) -> MachineLogic<Value> {
    MachineLogic::dynamic().guard("g", move |_, _, _| {
        on_guard();
        Ok(true)
    })
}

/// Save `key` as a `timer` machine started at wall time `T0` (deadline `T0 + 1`).
fn seed(store: &MemoryStore, key: &str, machine: &Machine, logic: &MachineLogic<Value>) {
    let clock = SimulatedClock::new();
    clock.set(T0 * 1000.0);
    let options =
        PersistedOptions::<Value> { clock: Arc::new(clock), ..PersistedOptions::default() };
    persisted(store, key, machine, logic, &options, |_, _| Ok(())).unwrap();
}

fn seed_timer(store: &MemoryStore, key: &str) {
    seed(store, key, &timer_machine(), &MachineLogic::dynamic());
}

fn single_chart(machine: Machine, logic: MachineLogic<Value>) -> MachineFactory<Value> {
    Arc::new(move |_| Some((machine.clone(), logic.clone())))
}

fn scanner(store: &Arc<MemoryStore>) -> DueTimerScanner<Value> {
    DueTimerScanner::new(
        Arc::clone(store) as Arc<dyn StateStore>,
        single_chart(timer_machine(), MachineLogic::dynamic()),
    )
}

fn states(store: &MemoryStore, key: &str) -> Vec<String> {
    stored_snapshot(store, key).state_ids
}

// -------------------------------------------------------------------------------- waking

#[test]
fn a_matured_deadline_wakes_the_machine_and_the_record_is_saved_expired() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
    assert_eq!(store.load("t:1").unwrap().unwrap().deadlines.len(), 1);

    let result = scanner(&store).scan_at(T0 + 2.0).unwrap();

    assert_eq!((result.scanned, result.due, result.woken), (1, 1, 1));
    assert_eq!(result.skipped_stale, 0);
    assert!(result.errors.is_empty());
    assert!((result.max_lag_s - 1.0).abs() < 1e-6, "woke 1s after the deadline: {result:?}");
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
    assert_eq!(version_of(store.as_ref(), "t:1"), 2);
    assert!(store.load("t:1").unwrap().unwrap().deadlines.is_empty(), "deadline consumed");

    let again = scanner(&store).scan_at(T0 + 3.0).unwrap();
    assert_eq!((again.due, again.woken), (0, 0), "nothing left to fire");
}

#[test]
fn the_injected_now_drives_scan_and_run_once() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let mut s = scanner(&store);
    s.now = Some(Arc::new(|| T0 + 2.0));
    assert_eq!(s.due_keys().unwrap(), ["t:1"]);
    assert_eq!(s.run_once().unwrap(), 1);
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
}

#[test]
fn a_deadline_in_the_future_is_not_fired() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let result = scanner(&store).scan_at(T0 + 0.5).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale), (0, 0, 0));
    assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
    assert_eq!(version_of(store.as_ref(), "t:1"), 1, "untouched");
}

#[test]
fn skew_tolerance_fires_slightly_early_deadlines() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let mut s = scanner(&store);
    s.skew_tolerance_s = 0.4;
    assert_eq!(s.scan_at(T0 + 0.5).unwrap().woken, 0, "still 0.1s too early");
    s.skew_tolerance_s = 0.6;
    assert_eq!(s.scan_at(T0 + 0.5).unwrap().woken, 1);
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
}

#[test]
fn a_key_without_a_machine_is_skipped_and_left_alone() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    seed_timer(&store, "other:1");
    let factory: MachineFactory<Value> =
        Arc::new(|key| key.starts_with("t:").then(|| (timer_machine(), MachineLogic::dynamic())));
    let s = DueTimerScanner::new(Arc::clone(&store) as Arc<dyn StateStore>, factory);
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.due, result.woken, result.unmapped), (2, 1, 1));
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
    assert_eq!(states(&store, "other:1"), ["timer.waiting"]);
    assert_eq!(version_of(store.as_ref(), "other:1"), 1);
}

#[test]
fn one_failing_key_is_recorded_and_the_scan_continues() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "a:bad");
    seed_timer(&store, "b:good");
    let wrong = Machine::from_value(&json!({
        "id": "different", "initial": "x", "states": { "x": {} }
    }))
    .unwrap();
    let factory: MachineFactory<Value> = Arc::new(move |key| {
        if key == "a:bad" {
            Some((wrong.clone(), MachineLogic::dynamic()))
        } else {
            Some((timer_machine(), MachineLogic::dynamic()))
        }
    });
    let s = DueTimerScanner::new(Arc::clone(&store) as Arc<dyn StateStore>, factory);
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale), (2, 1, 0));
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].0, "a:bad");
    assert!(matches!(result.errors[0].1, StoreError::Engine(_)));
    assert_eq!(states(&store, "b:good"), ["timer.expired"]);
}

// ------------------------------------------------------------------------ stale and races

#[test]
fn a_store_level_conflict_during_the_wake_counts_as_stale() {
    let store = Arc::new(MemoryStore::new());
    let logic = guard_logic(|| {});
    seed(&store, "t:1", &guarded_timer_machine(), &logic);

    // The guard runs while the scanner holds a loaded copy; a rival writes in that window.
    let rival_store = Arc::clone(&store);
    let rivalled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&rivalled);
    let logic = guard_logic(move || {
        if !flag.swap(true, Ordering::SeqCst) {
            let current = rival_store.load("t:1").unwrap().unwrap();
            let options = SaveOptions {
                expected_version: None,
                machine_version: "",
                deadlines: &current.deadlines,
            };
            rival_store.save("t:1", &current.snapshot, &options).unwrap();
        }
    });
    let s = DueTimerScanner::new(
        Arc::clone(&store) as Arc<dyn StateStore>,
        single_chart(guarded_timer_machine(), logic),
    );
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale, result.locked), (1, 0, 1, 0));
    assert!(result.errors.is_empty(), "a lost race is not an error: {:?}", result.errors);
    assert_eq!(states(&store, "t:1"), ["timer.waiting"], "the fenced save was refused");
    assert_eq!(version_of(store.as_ref(), "t:1"), 2, "only the rival's write landed");

    // The next scan (no more rivals) fires it.
    assert_eq!(s.scan_at(T0 + 3.0).unwrap().woken, 1);
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
}

/// An optimistic strategy that lets a rival write in between the scanner's re-check and the
/// load `persisted` does: `fence` is called exactly there.
#[derive(Debug)]
struct RivalDuringFence {
    store: Arc<MemoryStore>,
    armed: AtomicBool,
}

impl LockStrategy for RivalDuringFence {
    fn acquire(
        &self,
        _: &dyn StateStore,
        _: &str,
    ) -> xstate_persistence::Result<Option<Box<dyn LockGuard>>> {
        Ok(None)
    }

    fn fence(&self, version: u64) -> xstate_persistence::Result<Option<u64>> {
        if self.armed.swap(false, Ordering::SeqCst) {
            let current = self.store.load("t:1")?.unwrap();
            let options = SaveOptions {
                expected_version: None,
                machine_version: "",
                deadlines: &current.deadlines,
            };
            self.store.save("t:1", &current.snapshot, &options)?;
        }
        Ok(Some(version))
    }
}

#[test]
fn a_record_that_moved_after_the_recheck_is_stale_not_double_fired() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let mut s = scanner(&store);
    s.lock = Box::new(RivalDuringFence { store: Arc::clone(&store), armed: AtomicBool::new(true) });
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.woken, result.skipped_stale), (0, 1), "{result:?}");
    assert!(result.errors.is_empty());
    assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
    assert_eq!(version_of(store.as_ref(), "t:1"), 2, "the scanner wrote nothing");
    assert_eq!(s.scan_at(T0 + 2.0).unwrap().woken, 1, "disarmed: the next scan wins");
}

#[test]
fn a_stale_index_entry_is_skipped_under_the_lock() {
    /// Reports every key as due, whatever its record says.
    struct StaleIndex(MemoryStore);
    impl StateStore for StaleIndex {
        fn load(&self, k: &str) -> xstate_persistence::Result<Option<StoredSnapshot>> {
            self.0.load(k)
        }
        fn save(&self, k: &str, s: &str, o: &SaveOptions<'_>) -> xstate_persistence::Result<u64> {
            self.0.save(k, s, o)
        }
        fn delete(&self, k: &str) -> xstate_persistence::Result<bool> {
            self.0.delete(k)
        }
        fn forget(&self, k: &str) -> xstate_persistence::Result<xstate_persistence::ForgetReport> {
            self.0.forget(k)
        }
        fn list_keys(&self, p: &str, l: usize) -> xstate_persistence::Result<Vec<String>> {
            self.0.list_keys(p, l)
        }
        fn lock(&self, k: &str, t: Duration) -> xstate_persistence::Result<Box<dyn LockGuard>> {
            self.0.lock(k, t)
        }
        fn health(&self) -> xstate_persistence::Result<Health> {
            self.0.health()
        }
        fn due_keys(&self, _: f64, _: usize) -> xstate_persistence::Result<Vec<String>> {
            self.0.list_keys("", 100)
        }
    }

    let inner = MemoryStore::new();
    seed_timer(&inner, "t:1");
    let store: Arc<dyn StateStore> = Arc::new(StaleIndex(inner));
    let s = DueTimerScanner::new(store, single_chart(timer_machine(), MachineLogic::dynamic()));
    // The record's own deadline (T0 + 1) is in the future at T0 + 0.5.
    let result = s.scan_at(T0 + 0.5).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale), (1, 0, 1));
}

// --------------------------------------------------------------------------- lock strategies

#[test]
fn pessimistic_scanner_does_not_deadlock_on_its_own_lock() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let mut s = scanner(&store);
    s.lock = Box::new(PessimisticLock { timeout: Duration::from_millis(200) });
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.woken, result.skipped_stale, result.locked), (1, 0, 0), "{result:?}");
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
    store.lock("t:1", Duration::from_millis(10)).expect("lock released after the wake");
}

#[test]
fn a_locked_key_is_skipped_and_counted_as_locked() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let _held = store.lock("t:1", Duration::from_secs(1)).unwrap();
    let mut s = scanner(&store);
    s.lock = Box::new(PessimisticLock { timeout: Duration::from_millis(30) });
    let result = s.scan_at(T0 + 2.0).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale, result.locked), (1, 0, 1, 1));
    assert!(result.errors.is_empty());
    assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
}

#[test]
fn no_lock_scanner_still_wakes() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let mut s = scanner(&store);
    s.lock = Box::new(NoLock);
    assert_eq!(s.scan_at(T0 + 2.0).unwrap().woken, 1);
    let mut o = scanner(&store);
    o.lock = Box::new(OptimisticLock::default());
    assert_eq!(o.scan_at(T0 + 2.0).unwrap().woken, 0);
}

// ------------------------------------------------------------------- discovery and batching

#[test]
fn prefix_and_limit_select_the_earliest_matching_keys() {
    let store = Arc::new(MemoryStore::new());
    // Later start = later deadline.
    for (key, start_s) in
        [("a:late", 1010.0), ("a:early", 1000.0), ("b:x", 990.0), ("a:mid", 1005.0)]
    {
        let clock = SimulatedClock::new();
        clock.set(start_s * 1000.0);
        let options =
            PersistedOptions::<Value> { clock: Arc::new(clock), ..PersistedOptions::default() };
        persisted(
            store.as_ref(),
            key,
            &timer_machine(),
            &MachineLogic::dynamic(),
            &options,
            |_, _| Ok(()),
        )
        .unwrap();
    }
    let mut s = scanner(&store);
    s.prefix = "a:".into();
    s.limit = 2;
    assert_eq!(s.scan_at(2000.0).unwrap().woken, 2);
    assert_eq!(states(&store, "a:early"), ["timer.expired"]);
    assert_eq!(states(&store, "a:mid"), ["timer.expired"]);
    assert_eq!(states(&store, "a:late"), ["timer.waiting"], "beyond the batch");
    assert_eq!(states(&store, "b:x"), ["timer.waiting"], "other prefix");
}

/// A store with no deadline index: the scanner must fall back to loading records.
struct NoIndex(MemoryStore);

impl StateStore for NoIndex {
    fn load(&self, k: &str) -> xstate_persistence::Result<Option<StoredSnapshot>> {
        self.0.load(k)
    }
    fn save(&self, k: &str, s: &str, o: &SaveOptions<'_>) -> xstate_persistence::Result<u64> {
        self.0.save(k, s, o)
    }
    fn delete(&self, k: &str) -> xstate_persistence::Result<bool> {
        self.0.delete(k)
    }
    fn forget(&self, k: &str) -> xstate_persistence::Result<xstate_persistence::ForgetReport> {
        self.0.forget(k)
    }
    fn list_keys(&self, p: &str, l: usize) -> xstate_persistence::Result<Vec<String>> {
        self.0.list_keys(p, l)
    }
    fn lock(&self, k: &str, t: Duration) -> xstate_persistence::Result<Box<dyn LockGuard>> {
        self.0.lock(k, t)
    }
    fn health(&self) -> xstate_persistence::Result<Health> {
        self.0.health()
    }
}

#[test]
fn stores_without_an_index_are_scanned_record_by_record_earliest_first() {
    let inner = MemoryStore::new();
    // `z:` sorts last by key but is the earliest deadline: it must not be starved by `limit`.
    for (key, start_s) in [("a:1", 1010.0), ("m:1", 1005.0), ("z:1", 1000.0)] {
        let clock = SimulatedClock::new();
        clock.set(start_s * 1000.0);
        let options =
            PersistedOptions::<Value> { clock: Arc::new(clock), ..PersistedOptions::default() };
        persisted(&inner, key, &timer_machine(), &MachineLogic::dynamic(), &options, |_, _| Ok(()))
            .unwrap();
    }
    let store = Arc::new(NoIndex(inner));
    let mut s = DueTimerScanner::new(
        Arc::clone(&store) as Arc<dyn StateStore>,
        single_chart(timer_machine(), MachineLogic::dynamic()),
    );
    s.limit = 1;
    let result = s.scan_at(2000.0).unwrap();
    assert_eq!(result.woken, 1);
    assert!(result.scanned >= 3, "every record was inspected: {result:?}");
    assert_eq!(stored_snapshot(store.as_ref(), "z:1").state_ids, ["timer.expired"]);
    assert_eq!(stored_snapshot(store.as_ref(), "a:1").state_ids, ["timer.waiting"]);
}

// ----------------------------------------------------------------------------- run_until

#[test]
fn run_until_scans_repeatedly_and_stops_when_asked() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let s = scanner(&store); // system clock: the T0 deadline is long past
    let stop = AtomicBool::new(false);
    let scans = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        scope.spawn(|| s.run_until(&stop, Duration::from_millis(5)));
        let started = std::time::Instant::now();
        while states(&store, "t:1") != ["timer.expired"]
            && started.elapsed() < Duration::from_secs(10)
        {
            scans.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Release);
    });
    assert_eq!(states(&store, "t:1"), ["timer.expired"]);
}

#[test]
fn run_until_returns_promptly_even_with_a_long_interval() {
    let store = Arc::new(MemoryStore::new());
    let s = scanner(&store);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let stop = Arc::clone(&stop);
        let finished = Arc::new(Mutex::new(false));
        let finished2 = Arc::clone(&finished);
        std::thread::scope(|scope| {
            let h = scope.spawn(|| {
                s.run_until(&stop, Duration::from_secs(3600));
                *finished2.lock().unwrap() = true;
            });
            std::thread::sleep(Duration::from_millis(60));
            stop.store(true, Ordering::Release);
            h.join().unwrap();
        });
        finished
    };
    assert!(*handle.lock().unwrap());
}

#[test]
fn last_result_exists_before_the_first_scan_and_follows_each_scan() {
    let store = Arc::new(MemoryStore::new());
    seed_timer(&store, "t:1");
    let s = scanner(&store);
    assert_eq!(s.last_result(), None, "nothing scanned yet");
    s.scan_at(T0 + 2.0).unwrap();
    let last = s.last_result().expect("a scan ran");
    assert_eq!((last.due, last.woken, last.errors), (1, 1, 0));
    s.scan_at(T0 + 3.0).unwrap();
    assert_eq!(s.last_result().map(|r| r.woken), Some(0));
}

#[test]
fn invalid_settings_are_refused_not_clamped() {
    let store = Arc::new(MemoryStore::new());
    for (skew, limit) in [(-1.0, 1), (f64::NAN, 1), (f64::INFINITY, 1), (0.0, 0)] {
        let mut s = scanner(&store);
        s.skew_tolerance_s = skew;
        s.limit = limit;
        assert!(matches!(s.validate(), Err(StoreError::Other(_))), "{skew} {limit}");
        assert!(matches!(s.scan_at(T0), Err(StoreError::Other(_))));
        assert!(matches!(s.due_keys(), Err(StoreError::Other(_))));
    }
    assert!(scanner(&store).validate().is_ok());
}

#[test]
fn a_zero_interval_is_refused_instead_of_spinning() {
    let store = Arc::new(MemoryStore::new());
    let s = scanner(&store);
    let stop = AtomicBool::new(false);
    assert!(matches!(s.run_until_checked(&stop, Duration::ZERO), Err(StoreError::Other(_))));
    s.run_until(&stop, Duration::ZERO); // logs and returns; must not hang
    assert!(s.last_result().is_none(), "nothing was scanned");
}

// ----------------------------------------------------------------------------- unit helpers

#[test]
fn deadline_helpers() {
    let store = MemoryStore::new();
    seed_timer(&store, "t:1");
    let record = store.load("t:1").unwrap().unwrap();
    let d = &record.deadlines[0];
    assert!((d.due_at_wall - (T0 + 1.0)).abs() < 1e-6);
    assert!((d.remaining_ms(T0) - 1000.0).abs() < 1e-3);
    assert!(d.remaining_ms(T0 + 5.0).abs() < f64::EPSILON, "never negative");
    assert!(d.is_due(T0 + 1.0) && !d.is_due(T0 + 0.999));
    assert_eq!(check_record(&record, T0 + 1.0), Some(d.due_at_wall));
    assert_eq!(check_record(&record, T0), None);
}

#[test]
fn remaining_ms_clamps_a_huge_due_time_instead_of_overflowing() {
    let mut d = Deadline {
        state_id: "m.a".into(),
        entry_seq: 1,
        due_at_wall: f64::MAX,
        delay_ms: json!(1),
        event_type: "after.1.m.a".into(),
    };
    let huge = d.remaining_ms(0.0);
    assert!(huge.is_finite());
    assert!((huge - MAX_REMAINING_MS).abs() < f64::EPSILON);
    d.due_at_wall = 1e300;
    assert!((d.remaining_ms(-1e300) - MAX_REMAINING_MS).abs() < f64::EPSILON);
    d.due_at_wall = 10.0;
    assert!(d.remaining_ms(f64::NAN).abs() < f64::EPSILON, "NaN now reads as overdue");
}

#[test]
fn fixed_wall_clock_pins_wall_time_but_keeps_monotonic_time_moving() {
    let clock = FixedWallClock::new(42.5);
    assert!((clock.wall_now() - 42.5).abs() < f64::EPSILON);
    let before = clock.now_ms();
    std::thread::sleep(Duration::from_millis(5));
    assert!(clock.now_ms() > before);
    assert!((clock.wall_now() - 42.5).abs() < f64::EPSILON);
}
