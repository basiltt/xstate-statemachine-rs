#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Conformance suite run against every built-in store, plus backend-specific layout tests.
//!
//! Cross-language (Python) checks live in `cross_language.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;
use xstate_core::snapshot::Deadline;
use xstate_persistence::{
    FileStore, MemoryStore, SaveOptions, StateStore, StoreError, StoredSnapshot, validate_key,
};

const DEFAULT_CAP: usize = 1024 * 1024;

/// A store under test plus whatever keeps its storage alive.
struct Fixture {
    store: Box<dyn StateStore>,
    _dir: Option<TempDir>,
}

fn memory(cap: usize) -> Fixture {
    Fixture { store: Box::new(MemoryStore::with_max_snapshot_bytes(cap)), _dir: None }
}

fn file(cap: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let options =
        xstate_persistence::FileStoreOptions::default().fsync(false).max_snapshot_bytes(cap);
    let store = FileStore::open_with(dir.path().join("store"), options).unwrap();
    Fixture { store: Box::new(store), _dir: Some(dir) }
}

#[cfg(feature = "sqlite")]
fn sqlite_file(cap: usize) -> Fixture {
    use xstate_persistence::{SqliteStore, SqliteStoreOptions};
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteStoreOptions::default().max_snapshot_bytes(cap);
    let store = SqliteStore::open_with(dir.path().join("db").join("x.sqlite3"), options).unwrap();
    Fixture { store: Box::new(store), _dir: Some(dir) }
}

#[cfg(feature = "sqlite")]
fn sqlite_memory(cap: usize) -> Fixture {
    use xstate_persistence::{SqliteStore, SqliteStoreOptions};
    let options = SqliteStoreOptions::default().max_snapshot_bytes(cap);
    Fixture { store: Box::new(SqliteStore::open_with(":memory:", options).unwrap()), _dir: None }
}

fn dl(state: &str, seq: u64, due: f64, delay: i64) -> Deadline {
    Deadline {
        state_id: state.to_owned(),
        entry_seq: seq,
        due_at_wall: due,
        delay_ms: json!(delay),
        event_type: format!("after.{delay}.{state}"),
    }
}

fn put(s: &dyn StateStore, key: &str, snapshot: &str) -> u64 {
    s.save(key, snapshot, &SaveOptions::default()).unwrap()
}

fn put_if(
    s: &dyn StateStore,
    key: &str,
    snapshot: &str,
    expected: Option<u64>,
) -> Result<u64, StoreError> {
    s.save(key, snapshot, &SaveOptions { expected_version: expected, ..SaveOptions::default() })
}

fn load(s: &dyn StateStore, key: &str) -> Option<StoredSnapshot> {
    s.load(key).unwrap()
}

// ------------------------------------------------------------------------------------
// Generic cases
// ------------------------------------------------------------------------------------

mod cases {
    use super::*;

    pub(super) fn round_trip(s: &dyn StateStore) {
        let deadlines =
            [dl("m.a", 3, 1_700_000_000.5, 1500), dl("m.b", 4, 1_700_000_900.25, 900_000)];
        let before = xstate_persistence::store::wall_now();
        let snapshot = r#"{"v":4,"note":"héllo 日本 😀 \"q\" \\ \n"}"#;
        let opts =
            SaveOptions { machine_version: "2.1", deadlines: &deadlines, ..SaveOptions::default() };
        assert_eq!(s.save("order:42", snapshot, &opts).unwrap(), 1);

        let rec = load(s, "order:42").unwrap();
        assert_eq!(rec.key, "order:42");
        assert_eq!(rec.snapshot, snapshot);
        assert_eq!(rec.version, 1);
        assert_eq!(rec.machine_version, "2.1");
        assert_eq!(rec.deadlines, deadlines.to_vec());
        assert!(rec.updated_at >= before - 1.0 && rec.updated_at <= before + 60.0);
        assert!(load(s, "missing").is_none());
    }

    pub(super) fn versions(s: &dyn StateStore) {
        assert_eq!(put(s, "k", "{}"), 1);
        assert_eq!(put(s, "k", "{}"), 2);
        assert_eq!(put(s, "k", "{}"), 3);
        assert!(s.delete("k").unwrap());
        assert!(!s.delete("k").unwrap(), "second delete finds nothing");
        assert!(load(s, "k").is_none());
        assert_eq!(put(s, "k", "{}"), 1, "version restarts after delete");
        assert_eq!(put(s, "other", "{}"), 1, "versions are per key");
    }

    pub(super) fn expected_version(s: &dyn StateStore) {
        // 0 = must not exist
        assert_eq!(put_if(s, "k", "a", Some(0)).unwrap(), 1);
        match put_if(s, "k", "b", Some(0)) {
            Err(StoreError::Conflict { key, expected, actual }) => {
                assert_eq!((key.as_str(), expected, actual), ("k", 0, Some(1)));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(load(s, "k").unwrap().snapshot, "a", "a conflict writes nothing");
        // matching version
        assert_eq!(put_if(s, "k", "b", Some(1)).unwrap(), 2);
        // stale version
        assert!(matches!(
            put_if(s, "k", "c", Some(1)),
            Err(StoreError::Conflict { expected: 1, actual: Some(2), .. })
        ));
        // expecting a version of a key that does not exist
        assert!(matches!(
            put_if(s, "nope", "x", Some(3)),
            Err(StoreError::Conflict { expected: 3, actual: None, .. })
        ));
        assert!(load(s, "nope").is_none());
        // unconditional
        assert_eq!(put_if(s, "k", "d", None).unwrap(), 3);
        assert_eq!(load(s, "k").unwrap().snapshot, "d");
    }

    pub(super) fn conflict_keeps_deadlines(s: &dyn StateStore) {
        let first = [dl("m.a", 1, 100.0, 10)];
        let opts = SaveOptions { deadlines: &first, ..SaveOptions::default() };
        s.save("k", "{}", &opts).unwrap();
        let second = [dl("m.b", 2, 200.0, 20)];
        let bad =
            SaveOptions { expected_version: Some(9), deadlines: &second, ..SaveOptions::default() };
        assert!(s.save("k", "{}", &bad).is_err());
        assert_eq!(load(s, "k").unwrap().deadlines, first.to_vec());
        // a successful save replaces the deadline set (and may clear it)
        put(s, "k", "{}");
        assert_eq!(load(s, "k").unwrap().deadlines, Vec::<Deadline>::new());
    }

    pub(super) fn list_keys(s: &dyn StateStore) {
        for k in ["b:1", "a:2", "a:1", "a_1", "a%1", "Key", "key", "é", "z", "\u{ff5e}", "😀"] {
            put(s, k, "{}");
        }
        // sorted by code point (not UTF-16 unit): U+FF5E < U+1F600
        assert_eq!(
            s.list_keys("", 100).unwrap(),
            ["Key", "a%1", "a:1", "a:2", "a_1", "b:1", "key", "z", "é", "\u{ff5e}", "😀"]
        );
        // the prefix is literal: no `%` / `_` wildcards
        assert_eq!(s.list_keys("a:", 100).unwrap(), ["a:1", "a:2"]);
        assert_eq!(s.list_keys("a_", 100).unwrap(), ["a_1"]);
        assert_eq!(s.list_keys("a%", 100).unwrap(), ["a%1"]);
        assert_eq!(s.list_keys("zz", 100).unwrap(), Vec::<String>::new());
        // silently truncated at the limit, after sorting
        assert_eq!(s.list_keys("", 3).unwrap(), ["Key", "a%1", "a:1"]);
        assert_eq!(s.list_keys("a", 2).unwrap(), ["a%1", "a:1"]);
        assert_eq!(s.list_keys("", 0).unwrap(), Vec::<String>::new());
    }

    pub(super) fn forget(s: &dyn StateStore) {
        let deadlines = [dl("m.a", 1, 100.0, 10), dl("m.b", 2, 200.0, 20)];
        let opts = SaveOptions { deadlines: &deadlines, ..SaveOptions::default() };
        s.save("k", "{}", &opts).unwrap();
        put(s, "keep", "{}");
        let report = s.forget("k").unwrap();
        assert_eq!(report.snapshots, 1);
        assert!(load(s, "k").is_none());
        assert!(load(s, "keep").is_some());
        let again = s.forget("k").unwrap();
        assert_eq!(again.snapshots, 0);
        assert_eq!(again.deadlines, 0);
        assert_eq!(put(s, "k", "{}"), 1, "a forgotten key starts over");
    }

    pub(super) fn lock_timeout(s: &dyn StateStore) {
        let guard = s.lock("k", Duration::from_secs(1)).unwrap();
        assert_eq!(guard.key(), "k");
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    let started = Instant::now();
                    let err = s.lock("k", Duration::from_millis(150)).unwrap_err();
                    match err {
                        StoreError::LockTimeout { key, timeout, .. } => {
                            assert_eq!(key, "k");
                            assert_eq!(timeout, Duration::from_millis(150));
                        }
                        other => panic!("expected LockTimeout, got {other:?}"),
                    }
                    assert!(started.elapsed() >= Duration::from_millis(100));
                })
                .join()
                .unwrap();
        });
        drop(guard);
        let again = s.lock("k", Duration::from_secs(1)).unwrap();
        assert_eq!(again.key(), "k");
    }

    pub(super) fn lock_released_for_waiter(s: &dyn StateStore) {
        let guard = s.lock("k", Duration::from_secs(1)).unwrap();
        thread::scope(|scope| {
            let waiter =
                scope.spawn(|| s.lock("k", Duration::from_secs(10)).map(|g| g.key().to_owned()));
            thread::sleep(Duration::from_millis(100));
            drop(guard);
            assert_eq!(waiter.join().unwrap().unwrap(), "k");
        });
    }

    pub(super) fn save_inside_lock(s: &dyn StateStore) {
        let guard = s.lock("k", Duration::from_secs(1)).unwrap();
        assert_eq!(put(s, "k", "inside"), 1, "the lock holder can write without deadlocking");
        assert_eq!(put(s, "k", "inside2"), 2);
        assert_eq!(load(s, "k").unwrap().snapshot, "inside2");
        drop(guard);
        assert_eq!(load(s, "k").unwrap().snapshot, "inside2", "the write survives the release");
    }

    pub(super) fn size_cap(s: &dyn StateStore, cap: usize) {
        assert_eq!(s.max_snapshot_bytes(), cap);
        let exact = "x".repeat(cap);
        assert_eq!(put(s, "ok", &exact), 1);
        let too_big = "x".repeat(cap + 1);
        match s.save("big", &too_big, &SaveOptions::default()) {
            Err(StoreError::SnapshotTooLarge { key, size, limit }) => {
                assert_eq!((key.as_str(), size, limit), ("big", cap + 1, cap));
            }
            other => panic!("expected SnapshotTooLarge, got {other:?}"),
        }
        assert!(load(s, "big").is_none(), "nothing is written when the cap trips");
        // the cap counts UTF-8 bytes, not characters
        let wide = "é".repeat(cap / 2 + 1);
        assert!(matches!(
            s.save("wide", &wide, &SaveOptions::default()),
            Err(StoreError::SnapshotTooLarge { .. })
        ));
    }

    pub(super) fn invalid_keys(s: &dyn StateStore) {
        let long = "k".repeat(201);
        for bad in ["", "a\0b", long.as_str()] {
            assert!(validate_key(bad).is_err());
            assert!(matches!(s.load(bad), Err(StoreError::InvalidKey(_))), "load {bad:?}");
            assert!(matches!(
                s.save(bad, "{}", &SaveOptions::default()),
                Err(StoreError::InvalidKey(_))
            ));
            assert!(matches!(s.delete(bad), Err(StoreError::InvalidKey(_))));
            assert!(matches!(s.forget(bad), Err(StoreError::InvalidKey(_))));
            assert!(matches!(
                s.lock(bad, Duration::from_millis(10)),
                Err(StoreError::InvalidKey(_))
            ));
        }
        let longest = "k".repeat(200);
        assert_eq!(put(s, &longest, "{}"), 1, "200 characters is allowed");
        assert_eq!(load(s, &longest).unwrap().key, longest);
        let wide = "é".repeat(200);
        assert_eq!(put(s, &wide, "{}"), 1, "the limit counts code points");
        assert_eq!(s.list_keys("é", 10).unwrap(), [wide]);
    }

    pub(super) fn machine_version(s: &dyn StateStore) {
        for bad in ["a\0b".to_owned(), "v".repeat(256)] {
            let opts = SaveOptions { machine_version: &bad, ..SaveOptions::default() };
            assert!(s.save("k", "{}", &opts).is_err());
        }
        let ok = "v".repeat(255);
        let opts = SaveOptions { machine_version: &ok, ..SaveOptions::default() };
        s.save("k", "{}", &opts).unwrap();
        assert_eq!(load(s, "k").unwrap().machine_version, ok);
        put(s, "k", "{}");
        assert_eq!(
            load(s, "k").unwrap().machine_version,
            "",
            "an unlabelled save clears the label"
        );
    }

    pub(super) fn deadline_rules(s: &dyn StateStore) {
        // a whole float delay is normalised to an integer
        let float_delay = Deadline { delay_ms: json!(2000.0), ..dl("m.a", 1, 5.5, 2000) };
        let opts = SaveOptions { deadlines: &[float_delay], ..SaveOptions::default() };
        s.save("k", "{}", &opts).unwrap();
        assert_eq!(load(s, "k").unwrap().deadlines[0].delay_ms, json!(2000));
        // records that could not be loaded back (here or in Python) are refused up front
        let bad = [
            Deadline { delay_ms: json!(-1), ..dl("m.a", 1, 5.0, 1) },
            Deadline { delay_ms: json!(1.5), ..dl("m.a", 1, 5.0, 1) },
            Deadline { due_at_wall: f64::NAN, ..dl("m.a", 1, 5.0, 1) },
            Deadline { state_id: String::new(), ..dl("m.a", 1, 5.0, 1) },
            Deadline { entry_seq: u64::MAX, ..dl("m.a", 1, 5.0, 1) },
        ];
        for d in bad {
            let opts =
                SaveOptions { deadlines: std::slice::from_ref(&d), ..SaveOptions::default() };
            assert!(s.save("bad", "{}", &opts).is_err(), "{d:?}");
        }
        assert!(load(s, "bad").is_none());
    }

    pub(super) fn concurrent_create(s: &dyn StateStore) {
        let wins = AtomicUsize::new(0);
        let conflicts = AtomicUsize::new(0);
        thread::scope(|scope| {
            for i in 0..8 {
                let (wins, conflicts) = (&wins, &conflicts);
                scope.spawn(move || match put_if(s, "race", &format!("w{i}"), Some(0)) {
                    Ok(1) => {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(StoreError::Conflict { .. }) => {
                        conflicts.fetch_add(1, Ordering::SeqCst);
                    }
                    other => panic!("unexpected {other:?}"),
                });
            }
        });
        assert_eq!((wins.load(Ordering::SeqCst), conflicts.load(Ordering::SeqCst)), (1, 7));
        assert_eq!(load(s, "race").unwrap().version, 1);
    }

    pub(super) fn concurrent_increment(s: &dyn StateStore) {
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..5 {
                        put(s, "ctr", "{}");
                    }
                });
            }
        });
        assert_eq!(load(s, "ctr").unwrap().version, 20);
    }

    pub(super) fn health(s: &dyn StateStore, backend: &str) {
        let h = s.health().unwrap();
        assert!(h.ok);
        assert_eq!(h.backend, backend);
    }

    pub(super) fn due_keys(s: &dyn StateStore) {
        let save = |key: &str, deadlines: &[Deadline]| {
            s.save(key, "{}", &SaveOptions { deadlines, ..SaveOptions::default() }).unwrap();
        };
        save("late", &[dl("m.a", 1, 500.0, 1)]);
        save("early", &[dl("m.a", 1, 100.0, 1)]);
        save("tie-b", &[dl("m.a", 1, 200.0, 1)]);
        save("tie-a", &[dl("m.a", 1, 200.0, 1)]);
        save("two", &[dl("m.a", 1, 900.0, 1), dl("m.b", 2, 150.0, 1)]);
        save("none", &[]);
        // earliest deadline first, ties by key; a key appears once however many deadlines it has
        assert_eq!(s.due_keys(1000.0, 10).unwrap(), ["early", "two", "tie-a", "tie-b", "late"]);
        assert_eq!(s.due_keys(200.0, 10).unwrap(), ["early", "two", "tie-a", "tie-b"]);
        assert_eq!(s.due_keys(99.9, 10).unwrap(), Vec::<String>::new());
        assert_eq!(s.due_keys(1000.0, 2).unwrap(), ["early", "two"]);
        assert_eq!(s.due_keys(1000.0, 0).unwrap(), Vec::<String>::new());
        // re-saving without deadlines retires the key; deleting does too
        save("early", &[]);
        s.delete("tie-a").unwrap();
        assert_eq!(s.due_keys(200.0, 10).unwrap(), ["two", "tie-b"]);
    }
}

macro_rules! conformance {
    ($backend:ident, $make:path, $name:literal) => {
        mod $backend {
            use super::*;

            fn fx() -> Fixture {
                $make(DEFAULT_CAP)
            }

            #[test]
            fn round_trip() {
                cases::round_trip(&*fx().store);
            }
            #[test]
            fn versions() {
                cases::versions(&*fx().store);
            }
            #[test]
            fn expected_version() {
                cases::expected_version(&*fx().store);
            }
            #[test]
            fn conflict_keeps_deadlines() {
                cases::conflict_keeps_deadlines(&*fx().store);
            }
            #[test]
            fn list_keys() {
                cases::list_keys(&*fx().store);
            }
            #[test]
            fn forget() {
                cases::forget(&*fx().store);
            }
            #[test]
            fn lock_timeout() {
                cases::lock_timeout(&*fx().store);
            }
            #[test]
            fn lock_released_for_waiter() {
                cases::lock_released_for_waiter(&*fx().store);
            }
            #[test]
            fn save_inside_lock() {
                cases::save_inside_lock(&*fx().store);
            }
            #[test]
            fn size_cap() {
                cases::size_cap(&*$make(64).store, 64);
            }
            #[test]
            fn invalid_keys() {
                cases::invalid_keys(&*fx().store);
            }
            #[test]
            fn machine_version() {
                cases::machine_version(&*fx().store);
            }
            #[test]
            fn deadline_rules() {
                cases::deadline_rules(&*fx().store);
            }
            #[test]
            fn concurrent_create() {
                cases::concurrent_create(&*fx().store);
            }
            #[test]
            fn concurrent_increment() {
                cases::concurrent_increment(&*fx().store);
            }
            #[test]
            fn health() {
                cases::health(&*fx().store, $name);
            }
        }
    };
}

conformance!(memory_store, memory, "memory");
conformance!(file_store, file, "file");
#[cfg(feature = "sqlite")]
conformance!(sqlite_file_store, sqlite_file, "sqlite");
#[cfg(feature = "sqlite")]
conformance!(sqlite_memory_store, sqlite_memory, "sqlite");

#[test]
fn memory_due_keys() {
    cases::due_keys(&*memory(DEFAULT_CAP).store);
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_due_keys() {
    cases::due_keys(&*sqlite_file(DEFAULT_CAP).store);
    cases::due_keys(&*sqlite_memory(DEFAULT_CAP).store);
}

#[test]
fn file_store_has_no_deadline_index() {
    let fx = file(DEFAULT_CAP);
    assert!(matches!(fx.store.due_keys(1.0, 10), Err(StoreError::Other(_))));
}

#[test]
fn memory_forget_reports_deadlines() {
    let s = MemoryStore::new();
    let deadlines = [dl("m.a", 1, 1.0, 1), dl("m.b", 2, 2.0, 1)];
    s.save("k", "{}", &SaveOptions { deadlines: &deadlines, ..SaveOptions::default() }).unwrap();
    let report = s.forget("k").unwrap();
    assert_eq!((report.snapshots, report.deadlines), (1, 2));
}

#[test]
fn memory_health_counts_keys() {
    let s = MemoryStore::new();
    put(&s, "a", "{}");
    put(&s, "b", "{}");
    let h = s.health().unwrap();
    assert!(h.details.contains(&("keys".to_owned(), "2".to_owned())));
}
