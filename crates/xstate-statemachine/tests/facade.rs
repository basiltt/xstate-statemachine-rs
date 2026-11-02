//! The facade re-exports each crate under its feature and puts the everyday items in the
//! prelude. Each test is compiled only when its feature is on, so `cargo test -p
//! xstate-statemachine` passes with any feature subset.

use xstate_statemachine::prelude::*;

const TOGGLE: &str = r#"{
  "id": "toggle", "initial": "inactive",
  "states": { "inactive": { "on": { "TOGGLE": "active" } },
              "active":   { "on": { "TOGGLE": "inactive" } } }
}"#;

#[test]
fn prelude_runs_the_engine_without_any_feature() {
    let machine = Machine::from_json(TOGGLE).unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().unwrap();
    interp.send(Event::new("TOGGLE")).unwrap();
    assert_eq!(interp.state_ids(), ["toggle.active"]);
    let snapshot: Snapshot = interp.snapshot().unwrap();
    assert_eq!(snapshot.state_ids, ["toggle.active"]);
}

#[test]
fn core_module_is_the_engine_crate() {
    let machine: xstate_statemachine::core::Machine =
        xstate_statemachine::Machine::from_json(TOGGLE).unwrap();
    assert_eq!(machine.id(), "toggle");
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "current_thread")]
async fn tokio_feature_exposes_spawn_and_interpreter() {
    let machine = Machine::from_json(TOGGLE).unwrap();
    let actor: Interpreter =
        spawn(machine, MachineLogic::dynamic(), xstate_statemachine::tokio::Options::default())
            .unwrap();
    actor.send_and_wait("TOGGLE").await.unwrap();
    assert_eq!(actor.view().state_ids, ["toggle.active"]);
    actor.stop().await;
}

#[cfg(feature = "persistence")]
#[test]
fn persistence_feature_exposes_persisted_and_state_store() {
    use xstate_statemachine::persistence::{MemoryStore, PersistedOptions};

    let machine = Machine::from_json(TOGGLE).unwrap();
    let store = MemoryStore::new();
    let ids = persisted(
        &store,
        "t-1",
        &machine,
        &MachineLogic::dynamic(),
        &PersistedOptions::default(),
        |interp, _| {
            interp.send("TOGGLE")?;
            Ok(interp.state_ids())
        },
    )
    .unwrap();
    assert_eq!(ids, ["toggle.active"]);
    let dynamic: &dyn StateStore = &store;
    assert!(dynamic.load("t-1").unwrap().is_some());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_feature_exposes_the_sqlite_store() {
    let store = xstate_statemachine::persistence::SqliteStore::open_in_memory();
    assert!(store.is_ok());
}

#[cfg(feature = "eda")]
#[test]
fn eda_feature_round_trips_an_envelope() {
    use xstate_statemachine::eda::Envelope;

    let envelope = Envelope::new("order.paid").subject("order-1").build().unwrap();
    let wire = envelope.to_json(1 << 20).unwrap();
    assert_eq!(Envelope::from_json(&wire, 1 << 20).unwrap().ty, "order.paid");
}

#[cfg(feature = "patterns")]
#[test]
fn patterns_feature_exposes_the_retry_policy() {
    use xstate_statemachine::patterns::{JitterMode, RetryPolicy};

    let policy = RetryPolicy::new(3, 100.0, 2.0, 1_000.0, JitterMode::None).unwrap();
    assert!((policy.delay_for(2).unwrap() - 200.0).abs() < f64::EPSILON);
}

#[cfg(feature = "observability")]
#[test]
fn observability_feature_exposes_the_metrics_observer() {
    let _observer = MetricsObserver::new();
}

#[cfg(feature = "inspect")]
#[test]
fn inspect_feature_streams_messages_to_a_sink() {
    use std::sync::Arc;
    use xstate_statemachine::inspect::MemorySink;

    let sink = Arc::new(MemorySink::new());
    let mut interp =
        SyncInterpreter::new(Machine::from_json(TOGGLE).unwrap(), MachineLogic::dynamic());
    interp.observe(Arc::new(InspectorObserver::with_shared_sink(sink.clone())));
    interp.start().unwrap();
    assert_eq!(sink.messages().len(), 3); // actor, event, snapshot
}

#[cfg(feature = "macros")]
#[test]
fn macros_feature_exposes_derives_and_the_dsl() {
    #[derive(TypedEvent)]
    #[event(rename_all = "SCREAMING_SNAKE_CASE")]
    enum ToggleEvent {
        Toggle,
    }

    let machine = statechart! {
        id: "toggle",
        initial: inactive,
        states: {
            inactive { on { TOGGLE => active } }
            active { on { TOGGLE => inactive } }
        }
    }
    .unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().unwrap();
    interp.send(ToggleEvent::Toggle).unwrap();
    assert_eq!(interp.state_ids(), ["toggle.active"]);
}

#[cfg(feature = "agents")]
#[test]
fn agents_feature_exposes_the_tool_loop_chart() {
    assert!(xstate_statemachine::agents::CHART_NAMES.contains(&"tool_loop"));
}

#[cfg(feature = "axum")]
#[test]
fn axum_feature_exposes_the_registry() {
    use std::sync::Arc;
    use xstate_statemachine::axum::{RegisterOptions, StatechartRegistry};
    use xstate_statemachine::persistence::MemoryStore;

    let registry = StatechartRegistry::builder(Arc::new(MemoryStore::new()))
        .register(
            "toggle",
            Machine::from_json(TOGGLE).unwrap(),
            MachineLogic::dynamic(),
            RegisterOptions::default(),
        )
        .unwrap()
        .build();
    assert!(registry.is_ok());
}

#[cfg(feature = "testing")]
#[test]
fn testing_feature_reexports_the_crate() {
    assert_eq!(
        xstate_statemachine::testing::SNAPSHOT_VERSION,
        xstate_statemachine::SNAPSHOT_VERSION
    );
}
