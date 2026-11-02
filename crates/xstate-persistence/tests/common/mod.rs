//! Shared fixtures for the persistence integration tests.
#![allow(dead_code, clippy::print_stderr, clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use xstate_core::{Machine, MachineLogic, Snapshot};
use xstate_persistence::StateStore;

/// A counter machine: `INC` bumps `context.count`. `version` is the chart label.
pub(crate) fn counter_machine(version: Option<&str>) -> Machine {
    let mut doc = json!({
        "id": "counter",
        "initial": "idle",
        "context": { "count": 0 },
        "states": { "idle": { "on": { "INC": { "actions": "inc" } } } }
    });
    if let Some(v) = version {
        doc["version"] = json!(v);
    }
    Machine::from_value(&doc).unwrap()
}

/// Logic for [`counter_machine`].
pub(crate) fn counter_logic() -> MachineLogic<Value> {
    MachineLogic::dynamic().action("inc", |scope| {
        let n = scope.context["count"].as_i64().unwrap_or(0);
        scope.context["count"] = json!(n + 1);
        Ok(())
    })
}

/// The stored snapshot of `key`.
pub(crate) fn stored_snapshot(store: &dyn StateStore, key: &str) -> Snapshot {
    Snapshot::from_json(&store.load(key).unwrap().unwrap().snapshot).unwrap()
}

/// `context.count` of the stored record.
pub(crate) fn count_of(store: &dyn StateStore, key: &str) -> i64 {
    stored_snapshot(store, key).context["count"].as_i64().unwrap()
}

/// Record version of `key` (`0` when absent).
pub(crate) fn version_of(store: &dyn StateStore, key: &str) -> u64 {
    store.load(key).unwrap().map_or(0, |r| r.version)
}

/// A test that needs a docker-backed service (Redis, PostgreSQL) found it down: skip it with a
/// note, or fail it when `XSM_REQUIRE_SERVICES=1` is set (CI), so an unreachable service cannot
/// turn into silently missing coverage.
pub(crate) fn service_unavailable(test: &str, why: &str) {
    assert!(
        std::env::var("XSM_REQUIRE_SERVICES").as_deref() != Ok("1"),
        "`{test}`: {why}; XSM_REQUIRE_SERVICES=1 forbids skipping the test"
    );
    eprintln!("skipping `{test}`: {why}");
}
