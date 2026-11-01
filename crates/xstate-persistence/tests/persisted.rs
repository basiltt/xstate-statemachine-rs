#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `persisted`, the lock strategies and the load / save helpers.

mod common;

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{count_of, counter_logic, counter_machine, stored_snapshot, version_of};
use serde_json::{Value, json};
use xstate_core::Error as EngineError;
use xstate_persistence::locking::{
    DEFAULT_BACKOFF, Jitter, LockStrategy, NoLock, OptimisticLock, PersistedOptions,
    PessimisticLock, RetryPolicy, VersionMismatch, load_interpreter, persisted, persisted_retry,
    save_interpreter,
};
use xstate_persistence::memory_store::MemoryStore;
use xstate_persistence::{SaveOptions, StateStore, StoreError};

const KEY: &str = "order:1";

fn opts() -> PersistedOptions<Value> {
    PersistedOptions::default()
}

fn opts_with(lock: impl LockStrategy + 'static) -> PersistedOptions<Value> {
    PersistedOptions { lock: Box::new(lock), ..PersistedOptions::default() }
}

fn no_backoff(retries: u32) -> OptimisticLock {
    OptimisticLock {
        retries,
        backoff: RetryPolicy { base_ms: 0.0, jitter: Jitter::None, ..DEFAULT_BACKOFF },
    }
}

/// Create the record at version 1 with `count == 0`.
fn seed(store: &MemoryStore) {
    persisted(store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, _| Ok(()))
        .unwrap();
}

/// Another writer increments the counter, unconditionally.
fn rival_increment(store: &MemoryStore) {
    persisted(store, KEY, &counter_machine(None), &counter_logic(), &opts_with(NoLock), |i, _| {
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
}

// ------------------------------------------------------------------------ persisted basics

#[test]
fn clean_exit_saves_and_bumps_the_version_even_without_sends() {
    let store = MemoryStore::new();
    let machine = counter_machine(Some("1.0"));
    for expected in 1..=3 {
        persisted(&store, KEY, &machine, &counter_logic(), &opts(), |_, _| Ok(())).unwrap();
        assert_eq!(version_of(&store, KEY), expected);
    }
    let record = store.load(KEY).unwrap().unwrap();
    assert_eq!(record.machine_version, "1.0");
}

#[test]
fn state_changes_made_by_the_body_are_persisted() {
    let store = MemoryStore::new();
    let machine = counter_machine(None);
    let out = persisted(&store, KEY, &machine, &counter_logic(), &opts(), |i, _| {
        i.send("INC")?;
        i.send("INC")?;
        Ok(i.state_ids())
    })
    .unwrap();
    assert_eq!(out, ["counter.idle"]);
    assert_eq!(count_of(&store, KEY), 2);
    persisted(&store, KEY, &machine, &counter_logic(), &opts(), |i, _| {
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count_of(&store, KEY), 3, "second block restores the first block's state");
}

#[test]
fn missing_key_without_create_if_missing_is_not_found() {
    let store = MemoryStore::new();
    let options = PersistedOptions { create_if_missing: false, ..opts() };
    let called = Cell::new(false);
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &options, |_, _| {
        called.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref k) if k == KEY), "{err:?}");
    assert!(!called.get());
    assert_eq!(version_of(&store, KEY), 0);
}

#[test]
fn a_body_error_writes_nothing() {
    let store = MemoryStore::new();
    seed(&store);
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |i, _| {
        i.send("INC")?;
        Err::<(), _>(StoreError::Other("boom".into()))
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::Other(ref m) if m == "boom"));
    assert_eq!(version_of(&store, KEY), 1, "no new version");
    assert_eq!(count_of(&store, KEY), 0, "the INC was discarded");
}

#[test]
fn a_body_error_on_a_new_key_creates_nothing() {
    let store = MemoryStore::new();
    let r = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, _| {
        Err::<(), _>(StoreError::Other("boom".into()))
    });
    assert!(r.is_err());
    assert!(store.load(KEY).unwrap().is_none());
}

#[test]
fn snapshot_deadlines_are_indexed_on_save() {
    let store = MemoryStore::new();
    let machine = xstate_core::Machine::from_value(&json!({
        "id": "t", "initial": "a",
        "states": { "a": { "after": { "1000": "b" } }, "b": {} }
    }))
    .unwrap();
    persisted(&store, KEY, &machine, &xstate_core::MachineLogic::dynamic(), &opts(), |_, _| Ok(()))
        .unwrap();
    let record = store.load(KEY).unwrap().unwrap();
    assert_eq!(record.deadlines.len(), 1);
    assert_eq!(store.due_keys(f64::MAX, 10).unwrap(), [KEY]);
}

#[test]
fn verify_hash_rejects_a_changed_structure_unless_disabled() {
    let store = MemoryStore::new();
    seed(&store);
    let changed = xstate_core::Machine::from_value(&json!({
        "id": "counter", "initial": "idle", "context": { "count": 0 },
        "states": { "idle": { "on": { "INC": { "actions": "inc" }, "RESET": "idle" } } }
    }))
    .unwrap();
    let err =
        persisted(&store, KEY, &changed, &counter_logic(), &opts(), |_, _| Ok(())).unwrap_err();
    assert!(matches!(err, StoreError::Engine(EngineError::Snapshot(_))), "{err:?}");
    let lax = PersistedOptions { verify_hash: false, ..opts() };
    persisted(&store, KEY, &changed, &counter_logic(), &lax, |_, _| Ok(())).unwrap();
}

// -------------------------------------------------------------------- optimistic strategy

#[test]
fn optimistic_retries_after_a_concurrent_writer() {
    let store = MemoryStore::new();
    seed(&store);
    let calls = Cell::new(0);
    OptimisticLock::default()
        .run(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |i| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                rival_increment(&store);
            }
            i.send("INC")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(calls.get(), 2, "the body re-ran on the reloaded state");
    assert_eq!(count_of(&store, KEY), 2, "rival's INC and ours, none lost");
    assert_eq!(version_of(&store, KEY), 3, "seed, rival, ours");
}

#[test]
fn optimistic_gives_up_after_retries_plus_one_attempts() {
    let store = MemoryStore::new();
    seed(&store);
    let calls = Cell::new(0);
    let err = no_backoff(2)
        .run(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_| {
            calls.set(calls.get() + 1);
            rival_increment(&store);
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    assert_eq!(calls.get(), 3, "retries + 1 attempts");
    assert_eq!(count_of(&store, KEY), 3, "only the rival's writes landed");
}

#[test]
fn zero_retries_fail_on_the_first_conflict() {
    let store = MemoryStore::new();
    seed(&store);
    let calls = Cell::new(0);
    let err = no_backoff(0)
        .run(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_| {
            calls.set(calls.get() + 1);
            rival_increment(&store);
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }));
    assert_eq!(calls.get(), 1);
}

#[test]
fn persisted_does_not_retry_under_optimistic() {
    let store = MemoryStore::new();
    seed(&store);
    let calls = Cell::new(0);
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, _| {
        calls.set(calls.get() + 1);
        rival_increment(&store);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }));
    assert_eq!(calls.get(), 1, "a block cannot be re-run; the caller retries");
}

#[test]
fn creating_a_key_twice_concurrently_conflicts_on_version_zero() {
    let store = MemoryStore::new();
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, _| {
        // Another writer creates the key while we hold a "new" interpreter.
        rival_increment(&store);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { expected: 0, actual: Some(1), .. }), "{err:?}");
}

#[test]
fn boxed_strategies_can_run_too() {
    let store = MemoryStore::new();
    let strategy: Box<dyn LockStrategy> = Box::new(no_backoff(1));
    let n = strategy
        .run(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |i| {
            i.send("INC")?;
            Ok(7)
        })
        .unwrap();
    assert_eq!(n, 7);
    assert_eq!(count_of(&store, KEY), 1);
}

// ------------------------------------------------------------------- pessimistic strategy

#[test]
fn pessimistic_lock_serialises_two_threads() {
    const PER_THREAD: u32 = 15;
    let store = Arc::new(MemoryStore::new());
    let in_flight = AtomicUsize::new(0);
    let max_in_flight = AtomicUsize::new(0);
    let machine = counter_machine(None);
    let logic = counter_logic();
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                let options = opts_with(PessimisticLock { timeout: Duration::from_secs(30) });
                for _ in 0..PER_THREAD {
                    persisted(store.as_ref(), KEY, &machine, &logic, &options, |i, _| {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        max_in_flight.fetch_max(now, Ordering::SeqCst);
                        i.send("INC")?;
                        std::thread::sleep(Duration::from_millis(1));
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                }
            });
        }
    });
    assert_eq!(max_in_flight.load(Ordering::SeqCst), 1, "bodies never overlapped");
    assert_eq!(count_of(store.as_ref(), KEY), i64::from(2 * PER_THREAD), "no lost update");
    assert_eq!(version_of(store.as_ref(), KEY), u64::from(2 * PER_THREAD));
}

#[test]
fn pessimistic_lock_times_out_while_another_holder_has_the_key() {
    let store = MemoryStore::new();
    let _held = store.lock(KEY, Duration::from_secs(1)).unwrap();
    let called = Cell::new(false);
    let options = opts_with(PessimisticLock { timeout: Duration::from_millis(30) });
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &options, |_, _| {
        called.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::LockTimeout { .. }), "{err:?}");
    assert!(!called.get());
}

#[test]
fn pessimistic_lock_is_released_after_the_block_even_on_error() {
    let store = MemoryStore::new();
    let options = opts_with(PessimisticLock { timeout: Duration::from_millis(200) });
    let r = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &options, |_, _| {
        Err::<(), _>(StoreError::Other("x".into()))
    });
    assert!(r.is_err());
    drop(store.lock(KEY, Duration::from_millis(10)).expect("released"));
}

#[test]
fn pessimistic_save_is_still_fenced_by_the_version() {
    // A lock that "expired": another writer slipped in while the body held it.
    let store = MemoryStore::new();
    seed(&store);
    let options = opts_with(PessimisticLock::default());
    let err = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &options, |_, _| {
        store.save(KEY, &store.load(KEY).unwrap().unwrap().snapshot, &SaveOptions::default())?;
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
}

// ------------------------------------------------------------------------------- no lock

#[test]
fn no_lock_is_last_writer_wins() {
    let store = MemoryStore::new();
    seed(&store);
    let calls = Cell::new(0);
    persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts_with(NoLock), |i, _| {
        calls.set(calls.get() + 1);
        rival_increment(&store);
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(version_of(&store, KEY), 3, "seed, rival, ours: no conflict raised");
    assert_eq!(count_of(&store, KEY), 1, "ours overwrote the rival's INC (single-writer only)");
}

// ------------------------------------------------------------------------ after_commit

#[test]
fn after_commit_callbacks_run_in_order_after_the_save_and_the_lock_release() {
    let store = Arc::new(MemoryStore::new());
    seed(&store);
    let log: Arc<Mutex<Vec<(String, u64)>>> = Arc::default();
    let options = opts_with(PessimisticLock::default());
    persisted(store.as_ref(), KEY, &counter_machine(None), &counter_logic(), &options, |i, s| {
        i.send("INC")?;
        for name in ["first", "second", "third"] {
            let (store, log) = (Arc::clone(&store), Arc::clone(&log));
            s.after_commit(move || {
                // The save is already visible and the key lock is free again.
                let version = store.load(KEY)?.unwrap().version;
                store.lock(KEY, Duration::from_millis(10))?;
                log.lock().unwrap().push((name.to_owned(), version));
                Ok(())
            });
        }
        assert!(log.lock().unwrap().is_empty(), "nothing runs before the block ends");
        Ok(())
    })
    .unwrap();
    let expected: Vec<(String, u64)> =
        ["first", "second", "third"].iter().map(|n| ((*n).to_owned(), 2)).collect();
    assert_eq!(*log.lock().unwrap(), expected);
}

#[test]
fn a_failing_after_commit_callback_is_logged_and_does_not_stop_the_rest() {
    let store = MemoryStore::new();
    let ran = Arc::new(AtomicUsize::new(0));
    persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, s| {
        let r1 = Arc::clone(&ran);
        s.after_commit(move || {
            r1.fetch_add(1, Ordering::SeqCst);
            Err(StoreError::Other("publish failed".into()))
        });
        let r2 = Arc::clone(&ran);
        s.after_commit(move || {
            r2.fetch_add(10, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    })
    .expect("the save is durable; callback errors do not fail the block");
    assert_eq!(ran.load(Ordering::SeqCst), 11);
    assert_eq!(version_of(&store, KEY), 1);
}

#[test]
fn after_commit_callbacks_are_dropped_when_the_body_or_the_save_fails() {
    let store = MemoryStore::new();
    seed(&store);
    let ran = Arc::new(AtomicUsize::new(0));
    let register = |s: &mut xstate_persistence::locking::Session| {
        let ran = Arc::clone(&ran);
        s.after_commit(move || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
    };
    let _ = persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, s| {
        register(s);
        Err::<(), _>(StoreError::Other("body".into()))
    });
    let conflict =
        persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, s| {
            register(s);
            rival_increment(&store);
            Ok(())
        });
    assert!(matches!(conflict, Err(StoreError::Conflict { .. })));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[test]
fn persisted_retry_runs_only_the_winning_attempts_callbacks() {
    let store = MemoryStore::new();
    seed(&store);
    let ran = Arc::new(AtomicUsize::new(0));
    let calls = Cell::new(0);
    let options = opts_with(no_backoff(3));
    persisted_retry(&store, KEY, &counter_machine(None), &counter_logic(), &options, |i, s| {
        calls.set(calls.get() + 1);
        let ran = Arc::clone(&ran);
        s.after_commit(move || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        if calls.get() == 1 {
            rival_increment(&store);
        }
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the conflicted attempt's callback was discarded");
    assert_eq!(count_of(&store, KEY), 2);
}

// ------------------------------------------------------------------------ version policy

fn seed_v1(store: &MemoryStore) {
    persisted(store, KEY, &counter_machine(Some("1.0")), &counter_logic(), &opts(), |i, _| {
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn version_mismatch_error_refuses_and_writes_nothing() {
    let store = MemoryStore::new();
    seed_v1(&store);
    let err =
        persisted(&store, KEY, &counter_machine(Some("2.0")), &counter_logic(), &opts(), |_, _| {
            Ok(())
        })
        .unwrap_err();
    match err {
        StoreError::Engine(EngineError::Snapshot(m)) => {
            assert!(m.contains("1.0") && m.contains("2.0"), "{m}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(version_of(&store, KEY), 1);
    assert_eq!(store.load(KEY).unwrap().unwrap().machine_version, "1.0");
}

#[test]
fn version_mismatch_warn_restores_as_is_and_relabels_on_save() {
    let store = MemoryStore::new();
    seed_v1(&store);
    let options = PersistedOptions { on_version_mismatch: VersionMismatch::Warn, ..opts() };
    persisted(&store, KEY, &counter_machine(Some("2.0")), &counter_logic(), &options, |i, _| {
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count_of(&store, KEY), 2);
    assert_eq!(store.load(KEY).unwrap().unwrap().machine_version, "2.0");
}

#[test]
fn version_mismatch_migrate_upcasts_the_snapshot() {
    let store = MemoryStore::new();
    seed_v1(&store);
    let options = PersistedOptions {
        on_version_mismatch: VersionMismatch::Migrate(Arc::new(|mut snap| {
            assert_eq!(snap.machine_version.as_deref(), Some("1.0"));
            snap.machine_version = Some("2.0".into());
            snap.context["count"] = json!(100);
            Ok(snap)
        })),
        ..opts()
    };
    persisted(&store, KEY, &counter_machine(Some("2.0")), &counter_logic(), &options, |i, _| {
        i.send("INC")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count_of(&store, KEY), 101, "migrated context, then our INC");
    assert_eq!(stored_snapshot(&store, KEY).machine_version.as_deref(), Some("2.0"));
}

#[test]
fn a_failing_migration_aborts_the_restore() {
    let store = MemoryStore::new();
    seed_v1(&store);
    let options = PersistedOptions {
        on_version_mismatch: VersionMismatch::Migrate(Arc::new(|_| {
            Err(StoreError::Other("no path".into()))
        })),
        ..opts()
    };
    let err = persisted(
        &store,
        KEY,
        &counter_machine(Some("2.0")),
        &counter_logic(),
        &options,
        |_, _| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::Other(ref m) if m == "no path"));
    assert_eq!(version_of(&store, KEY), 1);
}

#[test]
fn matching_or_missing_labels_are_not_a_mismatch() {
    let store = MemoryStore::new();
    // Unlabelled record, labelled machine: cannot be checked, restores (the hash still guards).
    seed(&store);
    persisted(&store, KEY, &counter_machine(Some("2.0")), &counter_logic(), &opts(), |_, _| Ok(()))
        .unwrap();
    // Same label: fine. Labelled record, unlabelled machine: fine.
    persisted(&store, KEY, &counter_machine(Some("2.0")), &counter_logic(), &opts(), |_, _| Ok(()))
        .unwrap();
    persisted(&store, KEY, &counter_machine(None), &counter_logic(), &opts(), |_, _| Ok(()))
        .unwrap();
    assert_eq!(version_of(&store, KEY), 4);
}

// ------------------------------------------------------------------------------- helpers

#[test]
fn load_and_save_interpreter_round_trip_with_versions() {
    let store = MemoryStore::new();
    let machine = counter_machine(None);
    let logic = counter_logic();
    let options = opts();
    let clock = Arc::clone(&options.clock);

    let (mut interp, version) =
        load_interpreter(&store, KEY, &machine, &logic, Arc::clone(&clock), &options).unwrap();
    assert_eq!(version, 0, "new key");
    interp.send("INC").unwrap();
    assert_eq!(save_interpreter(&store, KEY, &interp, Some(version)).unwrap(), 1);

    let (mut interp, version) =
        load_interpreter(&store, KEY, &machine, &logic, Arc::clone(&clock), &options).unwrap();
    assert_eq!(version, 1);
    interp.send("INC").unwrap();
    let stale = save_interpreter(&store, KEY, &interp, Some(0)).unwrap_err();
    assert!(matches!(stale, StoreError::Conflict { expected: 0, actual: Some(1), .. }));
    assert_eq!(save_interpreter(&store, KEY, &interp, None).unwrap(), 2, "unconditional");
    assert_eq!(count_of(&store, KEY), 2);

    let missing = PersistedOptions { create_if_missing: false, ..opts() };
    let err = load_interpreter(&store, "nope", &machine, &logic, clock, &missing).unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[test]
fn invalid_keys_are_rejected_by_the_store() {
    let store = MemoryStore::new();
    let err =
        persisted(&store, "", &counter_machine(None), &counter_logic(), &opts(), |_, _| Ok(()))
            .unwrap_err();
    assert!(matches!(err, StoreError::InvalidKey(_)));
}

// ------------------------------------------------------------------------- retry policy

fn fixed(r: f64) -> impl FnMut() -> f64 {
    move || r
}

#[test]
fn default_backoff_matches_python() {
    assert_eq!(DEFAULT_BACKOFF.max_attempts, 6);
    assert!((DEFAULT_BACKOFF.base_ms - 2.0).abs() < f64::EPSILON);
    assert!((DEFAULT_BACKOFF.factor - 2.0).abs() < f64::EPSILON);
    assert!((DEFAULT_BACKOFF.max_ms - 100.0).abs() < f64::EPSILON);
    assert_eq!(DEFAULT_BACKOFF.jitter, Jitter::Full);
    assert_eq!(OptimisticLock::default().retries, 5);
}

#[test]
fn exponential_delay_grows_and_is_capped() {
    let p = RetryPolicy { jitter: Jitter::None, ..DEFAULT_BACKOFF };
    let delays: Vec<f64> = (1..=8).map(|a| p.delay_ms(a, None, &mut fixed(0.9))).collect();
    assert_eq!(delays, [2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 100.0, 100.0]);
    assert!(
        (p.delay_ms(10_000, None, &mut fixed(0.0)) - 100.0).abs() < f64::EPSILON,
        "overflow caps"
    );
}

#[test]
fn full_and_equal_jitter_stay_in_their_ranges() {
    let full = RetryPolicy { jitter: Jitter::Full, ..DEFAULT_BACKOFF };
    assert!((full.delay_ms(3, None, &mut fixed(0.5)) - 4.0).abs() < 1e-9, "uniform [0, 8]");
    assert!(full.delay_ms(3, None, &mut fixed(0.0)).abs() < 1e-9);
    let equal = RetryPolicy { jitter: Jitter::Equal, ..DEFAULT_BACKOFF };
    assert!((equal.delay_ms(3, None, &mut fixed(0.0)) - 4.0).abs() < 1e-9, "exp / 2");
    assert!((equal.delay_ms(3, None, &mut fixed(1.0)) - 8.0).abs() < 1e-9, "exp");
}

#[test]
fn decorrelated_jitter_follows_the_previous_delay_within_bounds() {
    let p = RetryPolicy { jitter: Jitter::Decorrelated, ..DEFAULT_BACKOFF };
    assert!((p.delay_ms(1, None, &mut fixed(0.0)) - 2.0).abs() < 1e-9, "floor is base");
    assert!((p.delay_ms(1, None, &mut fixed(1.0)) - 6.0).abs() < 1e-9, "base * 3");
    assert!((p.delay_ms(2, Some(10.0), &mut fixed(1.0)) - 30.0).abs() < 1e-9);
    assert!((p.delay_ms(2, Some(1000.0), &mut fixed(1.0)) - 100.0).abs() < 1e-9, "capped");
    assert!((p.delay_ms(2, Some(f64::NAN), &mut fixed(1.0)) - 6.0).abs() < 1e-9, "NaN → base");
}

#[test]
fn faulty_rng_samples_never_produce_invalid_delays() {
    let p = RetryPolicy { jitter: Jitter::Full, ..DEFAULT_BACKOFF };
    for bad in [f64::NAN, -3.0, 7.0, f64::INFINITY] {
        let d = p.delay_ms(2, None, &mut fixed(bad));
        assert!((0.0..=4.0).contains(&d), "{bad} gave {d}");
    }
}
