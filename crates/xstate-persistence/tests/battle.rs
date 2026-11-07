#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Adversarial battle tests for `xstate-persistence` (0.1.0 pre-release pass).
//!
//! These extend the conformance suite in `stores.rs` / `store_layouts.rs` with the cases the
//! §6.2 matrix calls out that had no direct coverage: ~50 concurrent writers, lock release on
//! panic, durable timers fired from a second store instance, the `RestartTimers` matrix through
//! `persisted`, Windows path / reserved-name / long-key / lock-stamp edges, and a planted-invalid-
//! key limit bug in `list_keys`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::{Clock, Machine, MachineLogic, RestartTimers, SimulatedClock};
#[cfg(feature = "sqlite")]
use xstate_persistence::SqliteStore;
#[cfg(feature = "sqlite")]
use xstate_persistence::idempotency::SqliteInbox;
use xstate_persistence::idempotency::{InboxStore, MemoryInbox, scope};
use xstate_persistence::locking::{PersistedOptions, PessimisticLock, persisted};
use xstate_persistence::memory_store::MemoryStore;
use xstate_persistence::timers::{DueTimerScanner, MachineFactory};
use xstate_persistence::{FileStore, FileStoreOptions, SaveOptions, StateStore, StoreError};

const KEY: &str = "order:1";

fn timer_machine() -> Machine {
    Machine::from_value(&json!({
        "id": "timer", "initial": "waiting",
        "states": { "waiting": { "after": { "1000": "expired" } }, "expired": {} }
    }))
    .unwrap()
}

fn timer_chart() -> MachineFactory<Value> {
    let (machine, logic) = (timer_machine(), MachineLogic::dynamic());
    Arc::new(move |_| Some((machine.clone(), logic.clone())))
}

fn states(store: &dyn StateStore, key: &str) -> Vec<String> {
    common::stored_snapshot(store, key).state_ids
}

/// Arm a `timer` machine at wall time `t0` (deadline `t0 + 1`) using `store`.
fn seed_timer(store: &dyn StateStore, key: &str, t0: f64) {
    let clock = Arc::new(SimulatedClock::new());
    clock.set(t0 * 1000.0);
    let options = PersistedOptions::<Value> { clock, ..PersistedOptions::default() };
    persisted(store, key, &timer_machine(), &MachineLogic::dynamic(), &options, |_, _| Ok(()))
        .unwrap();
}

// ------------------------------------------------------------------------ ~50 concurrent writers

#[test]
fn fifty_pessimistic_writers_never_lose_an_update() {
    const WRITERS: usize = 50;
    let store = Arc::new(MemoryStore::new());
    let machine = common::counter_machine(None);
    let logic = common::counter_logic();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    thread::scope(|scope| {
        for _ in 0..WRITERS {
            let (store, machine, logic) = (Arc::clone(&store), machine.clone(), logic.clone());
            let (in_flight, max_in_flight) = (Arc::clone(&in_flight), Arc::clone(&max_in_flight));
            scope.spawn(move || {
                let options = PersistedOptions {
                    lock: Box::new(PessimisticLock { timeout: Duration::from_secs(60) }),
                    ..PersistedOptions::default()
                };
                persisted(store.as_ref(), KEY, &machine, &logic, &options, |i, _| {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_in_flight.fetch_max(now, Ordering::SeqCst);
                    i.send("INC")?;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            });
        }
    });
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 1, "pessimistic bodies never overlapped");
    assert_eq!(common::count_of(store.as_ref(), KEY), i64::try_from(WRITERS).expect("fits"));
    assert_eq!(common::version_of(store.as_ref(), KEY), WRITERS as u64);
}

#[test]
fn fifty_optimistic_creators_have_exactly_one_winner() {
    let store = Arc::new(MemoryStore::new());
    let wins = AtomicUsize::new(0);
    let conflicts = AtomicUsize::new(0);
    thread::scope(|scope| {
        for i in 0..50 {
            let (store, wins, conflicts) = (Arc::clone(&store), &wins, &conflicts);
            scope.spawn(move || {
                let options = SaveOptions { expected_version: Some(0), ..SaveOptions::default() };
                match store.save("race", &format!("w{i}"), &options) {
                    Ok(1) => {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(StoreError::Conflict { .. }) => {
                        conflicts.fetch_add(1, Ordering::SeqCst);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            });
        }
    });
    assert_eq!((wins.load(Ordering::SeqCst), conflicts.load(Ordering::SeqCst)), (1, 49));
    assert_eq!(common::version_of(store.as_ref(), "race"), 1);
}

// ------------------------------------------------------------------- lock release on panic

#[test]
fn memory_lock_is_released_when_the_holder_panics() {
    let store = MemoryStore::new();
    let guard = store.lock(KEY, Duration::from_secs(1)).unwrap();
    let handle = thread::spawn(move || {
        let _guard = guard;
        panic!("the holder dies mid-critical-section");
    });
    assert!(handle.join().is_err(), "the holder thread panicked");
    drop(store.lock(KEY, Duration::from_secs(1)).unwrap_or_else(|e| panic!("{e}")));
}

#[test]
fn file_lock_is_released_when_the_holder_panics() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    let guard = store.lock(KEY, Duration::from_secs(1)).unwrap();
    let handle = thread::spawn(move || {
        let _guard = guard;
        panic!("the holder dies mid-critical-section");
    });
    assert!(handle.join().is_err());
    drop(store.lock(KEY, Duration::from_secs(2)).unwrap_or_else(|e| panic!("{e}")));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_lock_is_released_when_the_holder_panics() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("x.db")).unwrap();
    let guard = store.lock(KEY, Duration::from_secs(1)).unwrap();
    let handle = thread::spawn(move || {
        let _guard = guard;
        panic!("the holder dies mid-critical-section");
    });
    assert!(handle.join().is_err());
    // SQLite's database-wide lock is rolled back, not left held.
    drop(store.lock("other", Duration::from_secs(2)).unwrap_or_else(|e| panic!("{e}")));
}

// ------------------------------------------------------------ durable timers, second instance

#[test]
fn file_store_durable_timer_fires_from_a_second_instance() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store =
            FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
        seed_timer(&store, "t:1", 1000.0);
        assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
    } // the arming interpreter is dropped here
    let scanner_store =
        FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    let scanner =
        DueTimerScanner::new(Arc::new(scanner_store) as Arc<dyn StateStore>, timer_chart());
    let result = scanner.scan_at(2000.0).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale), (1, 1, 0), "{result:?}");
    assert!(result.errors.is_empty());
    let verify =
        FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    assert_eq!(states(&verify, "t:1"), ["timer.expired"]);
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_store_durable_timer_fires_from_a_second_instance() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    {
        let store = SqliteStore::open(&db).unwrap();
        seed_timer(&store, "t:1", 1000.0);
        assert_eq!(states(&store, "t:1"), ["timer.waiting"]);
    }
    let scanner_store = SqliteStore::open(&db).unwrap();
    let scanner =
        DueTimerScanner::new(Arc::new(scanner_store) as Arc<dyn StateStore>, timer_chart());
    let result = scanner.scan_at(2000.0).unwrap();
    assert_eq!((result.due, result.woken, result.skipped_stale), (1, 1, 0), "{result:?}");
    let verify = SqliteStore::open(&db).unwrap();
    assert_eq!(states(&verify, "t:1"), ["timer.expired"]);
}

// ------------------------------------------------------------- RestartTimers through persisted

#[test]
fn restart_timers_matrix_applies_through_persisted() {
    // FireDue: a matured deadline fires during the restore, before the body.
    {
        let store = MemoryStore::new();
        let clock = Arc::new(SimulatedClock::new());
        clock.set(1000.0 * 1000.0);
        let options = PersistedOptions::<Value> {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            ..PersistedOptions::default()
        };
        persisted(&store, KEY, &timer_machine(), &MachineLogic::dynamic(), &options, |_, _| Ok(()))
            .unwrap();
        clock.set(2000.0 * 1000.0);
        let fire = PersistedOptions::<Value> {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            restart_timers: RestartTimers::FireDue,
            ..PersistedOptions::default()
        };
        persisted(&store, KEY, &timer_machine(), &MachineLogic::dynamic(), &fire, |i, _| {
            assert_eq!(i.state_ids(), ["timer.expired"], "FireDue fired during restore");
            Ok(())
        })
        .unwrap();
        assert_eq!(states(&store, KEY), ["timer.expired"]);
    }
    // Parked: the deadline is retained but no timer is pending or fired.
    {
        let store = MemoryStore::new();
        seed_timer(&store, KEY, 1000.0);
        let clock = Arc::new(SimulatedClock::new());
        clock.set(2000.0 * 1000.0);
        let parked = PersistedOptions::<Value> {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            restart_timers: RestartTimers::Parked,
            ..PersistedOptions::default()
        };
        persisted(&store, KEY, &timer_machine(), &MachineLogic::dynamic(), &parked, |i, _| {
            assert_eq!(i.state_ids(), ["timer.waiting"], "Parked does not fire");
            assert_eq!(i.pending_timers(), 0, "Parked re-arms nothing");
            Ok(())
        })
        .unwrap();
        assert_eq!(states(&store, KEY), ["timer.waiting"]);
    }
    // Resume: the deadline is re-armed from the restored clock, not fired.
    {
        let store = MemoryStore::new();
        seed_timer(&store, KEY, 1000.0);
        let clock = Arc::new(SimulatedClock::new());
        clock.set(2000.0 * 1000.0);
        let resume = PersistedOptions::<Value> {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            restart_timers: RestartTimers::Resume,
            ..PersistedOptions::default()
        };
        persisted(&store, KEY, &timer_machine(), &MachineLogic::dynamic(), &resume, |i, _| {
            assert_eq!(i.state_ids(), ["timer.waiting"]);
            assert_eq!(i.pending_timers(), 1, "Resume re-armed the deadline");
            Ok(())
        })
        .unwrap();
    }
}

// -------------------------------------------------------------------- FileStore Windows edges

#[test]
fn file_lock_stamp_is_written_at_byte_one_with_the_holder_pid() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    let guard = store.lock("k", Duration::from_secs(1)).unwrap();
    assert_eq!(guard.key(), "k");
    // The lock file is locked while the guard is alive (reading it then is refused on Windows);
    // after the release the advisory lock is gone but the holder stamp remains on disk.
    drop(guard);
    let lock_path = dir.path().join("k.lock");
    let bytes = std::fs::read(&lock_path).unwrap();
    assert!(bytes.len() >= 64, "a 1-byte locked byte plus a 63-byte holder stamp: {bytes:?}");
    assert_eq!(bytes[0], 0, "byte 0 is the advisory-locked byte");
    let stamp = String::from_utf8_lossy(&bytes[1..]).trim().to_owned();
    assert!(
        stamp.starts_with(&std::process::id().to_string()),
        "the stamp names this process: {stamp:?}"
    );
    // the file survives (it is not deleted on release) and a second holder acquires the same key
    store.lock("k", Duration::from_secs(1)).unwrap();
}

#[test]
fn file_reserved_device_names_all_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    let mut keys: Vec<String> = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "con.txt",
        "nul.txt",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    for key in &keys {
        store.save(key, "{}", &SaveOptions::default()).unwrap();
    }
    keys.sort();
    assert_eq!(store.list_keys("", 100).unwrap(), keys);
    for key in &keys {
        assert_eq!(store.load(key).unwrap().unwrap().key, *key);
    }
    // the pure device names are written under a %5f-prefixed stem, never the raw device name
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".xsm.json"))
        .collect();
    for raw in ["con", "nul", "com1", "lpt9"] {
        assert!(
            !names.contains(&format!("{raw}.xsm.json")),
            "{raw} must never be a raw file name: {names:?}"
        );
    }
}

#[test]
fn file_long_keys_hash_at_the_boundary_and_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    let verbatim = "a".repeat(200);
    // 34 code points (a legal key) but a 204-character encoding: hashed.
    let hashed = "é".repeat(34);
    store.save(&verbatim, "{}", &SaveOptions::default()).unwrap();
    store.save(&hashed, "{}", &SaveOptions::default()).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&format!("{verbatim}.xsm.json")), "{names:?}");
    assert!(
        names.iter().any(|n| n.starts_with('~') && n.ends_with(".xsm.json")),
        "the 201-char key is hashed: {names:?}"
    );
    let mut keys = store.list_keys("", 10).unwrap();
    keys.sort();
    assert_eq!(keys, [verbatim.clone(), hashed.clone()]);
    assert_eq!(store.load(&hashed).unwrap().unwrap().key, hashed);
}

#[test]
fn file_windows_path_like_keys_are_refused_or_safely_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open_with(dir.path(), FileStoreOptions::default().fsync(false)).unwrap();
    for bad in ["C:\\x", "C:/x", "\\\\server\\share", "//server/share", "a\\b", "a/b"] {
        assert!(
            matches!(
                store.save(bad, "{}", &SaveOptions::default()),
                Err(StoreError::InvalidKey(_))
            ),
            "{bad:?}"
        );
        assert!(matches!(store.load(bad), Err(StoreError::InvalidKey(_))), "{bad:?}");
    }
    // Not separators and not an absolute path: legal keys that must round-trip.
    for ok in ["a:b", "...", "a..b", ".hidden", "..leading"] {
        store.save(ok, "{}", &SaveOptions::default()).unwrap();
        assert_eq!(store.load(ok).unwrap().unwrap().key, ok, "{ok:?}");
    }
    let records = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".xsm.json"))
        .count();
    assert_eq!(records, 5, "only the five legal records were written");
}

// -------------------------------------------------------------- planted invalid key in list_keys

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_list_keys_filters_invalid_keys_before_applying_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    let store = SqliteStore::open(&db).unwrap();
    store.save("a", "{}", &SaveOptions::default()).unwrap();
    store.save("b", "{}", &SaveOptions::default()).unwrap();
    // A foreign writer plants a key the store would refuse: '' sorts before everything.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO statecharts(key, snapshot, version, machine_version, updated_at) \
         VALUES ('', '{}', 1, '', 1.0)",
        [],
    )
    .unwrap();
    assert_eq!(
        store.list_keys("", 1).unwrap(),
        ["a"],
        "an invalid planted key must be filtered out before the LIMIT is applied"
    );
}

// --------------------------------------------------------------------- inbox concurrency/scope

fn inboxes() -> Vec<(&'static str, Arc<dyn InboxStore>)> {
    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
    let mut all: Vec<(&'static str, Arc<dyn InboxStore>)> =
        vec![("memory", Arc::new(MemoryInbox::new()))];
    #[cfg(feature = "sqlite")]
    all.push(("sqlite", Arc::new(SqliteInbox::open_in_memory().unwrap())));
    all
}

#[test]
fn fifty_concurrent_inbox_claims_have_exactly_one_winner() {
    for (name, inbox) in inboxes() {
        let wins = (0..50)
            .map(|n| {
                let inbox = Arc::clone(&inbox);
                thread::spawn(move || {
                    inbox.claim("s", "k", &format!("fp-{n}"), Some(60.0)).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| panic!("{name}: worker panicked")))
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1, "{name}");
    }
}

#[test]
fn scopes_that_would_collide_without_escaping_stay_apart() {
    for (name, inbox) in inboxes() {
        let a = scope("a/m", "k", "z");
        let b = scope("a", "m/k", "z");
        assert_ne!(a, b);
        assert!(inbox.claim(&a, "e", "fp", None).unwrap(), "{name}");
        assert!(inbox.claim(&b, "e", "fp", None).unwrap(), "{name}: no scope collision");
        assert!(inbox.get(&a, "e").unwrap().is_some(), "{name}");
        assert!(inbox.get(&b, "e").unwrap().is_some(), "{name}");
    }
}
