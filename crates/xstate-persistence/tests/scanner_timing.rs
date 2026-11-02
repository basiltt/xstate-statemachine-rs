#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `DueTimerScanner::run_until` must notice `stop` long before its scan interval elapses.
//!
//! `tests/scanner.rs` checks that the loop returns; this file bounds *how fast*, because an
//! hour-long interval that is only honoured at its end would pass a test with no timing in it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic};
use xstate_persistence::StateStore;
use xstate_persistence::memory_store::MemoryStore;
use xstate_persistence::timers::{DueTimerScanner, MachineFactory};

/// Generous for a loaded CI runner: the loop polls `stop` every few hundred milliseconds at most.
const PROMPT: Duration = Duration::from_secs(5);

#[test]
fn run_until_returns_within_five_seconds_of_stop_even_with_an_hour_long_interval() {
    let machine = Machine::from_value(&json!({
        "id": "timer", "initial": "waiting",
        "states": { "waiting": { "after": { "1000": "expired" } }, "expired": {} }
    }))
    .unwrap();
    let logic: MachineLogic<Value> = MachineLogic::dynamic();
    let chart: MachineFactory<Value> = Arc::new(move |_| Some((machine.clone(), logic.clone())));
    let store = Arc::new(MemoryStore::new());
    let scanner: DueTimerScanner<Value> =
        DueTimerScanner::new(Arc::clone(&store) as Arc<dyn StateStore>, chart);
    let stop = AtomicBool::new(false);

    let elapsed = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            scanner.run_until(&stop, Duration::from_secs(3600));
        });
        std::thread::sleep(Duration::from_millis(60));
        let stopped_at = Instant::now();
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        stopped_at.elapsed()
    });

    assert!(elapsed < PROMPT, "run_until took {elapsed:?} to notice `stop`; limit {PROMPT:?}");
}
