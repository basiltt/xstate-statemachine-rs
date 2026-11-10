//! # xstate-statemachine
//!
//! **Statecharts for Rust, compatible with [XState v5](https://stately.ai/docs/xstate) and the
//! [Stately editor](https://stately.ai/editor).** Design a chart visually, export it as JSON,
//! supply the named guards, actions and services in Rust, and run it: hierarchy, parallel
//! regions, history, delayed transitions, invoked services, actors, snapshots and durable
//! persistence. This crate is the Rust port of the Python library
//! [`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine); snapshots, store
//! layouts, event envelopes and inspector recordings are byte-compatible with it.
//!
//! This is the **facade**: one dependency that re-exports the engine and, behind feature flags
//! that mirror the Python library's extras, every other crate of the workspace. The engine is
//! always present; everything else is opt-in.
//!
//! ```toml
//! [dependencies]
//! xstate-statemachine = { version = "0.1", features = ["tokio", "sqlite", "inspect"] }
//! ```
//!
//! # Quick start
//!
//! ```
//! use xstate_statemachine::prelude::*;
//!
//! let machine = Machine::from_json(r#"{
//!   "id": "door", "initial": "closed", "context": {"opens": 0},
//!   "states": {
//!     "closed": {"on": {"OPEN": {"target": "opened", "guard": "isUnlocked", "actions": "countOpen"}}},
//!     "opened": {"on": {"CLOSE": "closed"}}
//!   }
//! }"#)?;
//! let logic = MachineLogic::dynamic()
//!     .guard("isUnlocked", |ctx, _event, _params| Ok(ctx["locked"] != true))
//!     .action("countOpen", |scope| {
//!         let n = scope.context["opens"].as_u64().unwrap_or(0) + 1;
//!         scope.context["opens"] = n.into();
//!         Ok(())
//!     });
//! let mut door = SyncInterpreter::new(machine, logic);
//! door.start()?;
//! door.send("OPEN")?;
//! assert_eq!(door.state_ids(), ["door.opened"]);
//! # Ok::<(), xstate_statemachine::core::Error>(())
//! ```
//!
//! # Features
//!
//! Every feature is off by default. Each one enables an optional dependency and adds a module
//! of the same name that re-exports the crate; [`prelude`] picks up the matching everyday
//! items. Backend features imply the feature they extend.
//!
//! | Feature | Enables | Python extra |
//! |---|---|---|
//! | *(always)* | `core`: machine model, JSON parser, step engine, `SyncInterpreter`, snapshots, pure API, graph utilities | core |
//! | `tokio` | `tokio`: async interpreter (`spawn`, mailbox, timers, services, actors) | core (asyncio engine) |
//! | `persistence` | `persistence`: `StateStore`, memory and file stores, locks, `persisted`, durable timers, inbox, log | core (persistence) |
//! | `sqlite` | SQLite store, inbox, log and outbox (implies `persistence`) | core (stdlib `sqlite3`) |
//! | `redis` | `RedisStore`, `RedisInbox`, `RedisLog` (implies `persistence`) | `redis` |
//! | `postgres` | `PostgresStore`, `PostgresInbox`, `PostgresLog` over sqlx (implies `persistence`) | `sqlalchemy` |
//! | `eda` | `eda`: CloudEvents envelope, broker contract, dispatcher, outbox, dead letters, AsyncAPI (implies `persistence`) | `eda` |
//! | `redis-streams` | Redis Streams broker adapter (implies `eda`) | `redis` |
//! | `nats` | NATS `JetStream` broker adapter (implies `eda`) | `nats` |
//! | `rabbitmq` | RabbitMQ (AMQP 0.9.1) broker adapter (implies `eda`) | `rabbitmq` |
//! | `sqs` | Amazon SQS broker adapter (implies `eda`) | `sqs` |
//! | `patterns` | `patterns`: retry policy, circuit breaker, saga builder, choreography router (implies `eda`) | core (`patterns`) |
//! | `observability` | `observability`: `MetricsObserver` over the `metrics` facade | `observability` |
//! | `inspect` | `inspect`: Stately Inspector protocol, JSONL recording and replay, SSE server | core (`inspect`) |
//! | `macros` | `macros`: `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `statechart!` | the Pythonic DSL |
//! | `axum` | `axum`: registry, REST / SSE / WebSocket routes, problem details (implies `persistence`, `inspect`) | `starlette` / `fastapi` |
//! | `agents` | `agents`: LLM agents as statecharts | `agents` |
//! | `testing` | `testing`: test helpers (a skeleton; graph walking and `stub_logic` live in `core`) | `testing` |
//! | `full` | every feature above | `all` |
//!
//!
//! # Async and persistence together
//!
//! ```
//! # #[cfg(all(feature = "tokio", feature = "persistence"))]
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use xstate_statemachine::persistence::{MemoryStore, PersistedOptions};
//! use xstate_statemachine::prelude::*;
//! use xstate_statemachine::tokio::Options;
//!
//! let machine = Machine::from_json(
//!     r#"{"id":"order","initial":"pending","states":{"pending":{"on":{"PAY":"paid"}},"paid":{}}}"#,
//! )?;
//!
//! // An async actor: a cheap, cloneable handle to a task that owns the state.
//! let actor = spawn(machine.clone(), MachineLogic::dynamic(), Options::default())?;
//! actor.send_and_wait("PAY").await?;
//! assert_eq!(actor.view().state_ids, ["order.paid"]);
//! actor.stop().await;
//!
//! // Durable: load -> act -> save, in a store a Python worker could share.
//! let store = MemoryStore::new();
//! let ids = persisted(&store, "order-1", &machine, &MachineLogic::dynamic(),
//!                     &PersistedOptions::default(), |interp, _session| {
//!     interp.send("PAY")?;
//!     Ok(interp.state_ids())
//! })?;
//! assert_eq!(ids, ["order.paid"]);
//! # Ok(()) }
//! # #[cfg(not(all(feature = "tokio", feature = "persistence")))]
//! # fn main() {}
//! ```
//!
//! # Where things live
//!
//! | Module | Crate | Python counterpart |
//! |---|---|---|
//! | [`core`] | `xstate-core` | `xstate_statemachine` (engine, snapshots, pure API, graph, `to_json` / `to_mermaid`) |
//! | `tokio` | `xstate-tokio` | `xstate_statemachine.interpreter` (asyncio engine) |
//! | `persistence` | `xstate-persistence` | `xstate_statemachine.persistence` |
//! | `eda` | `xstate-eda` | `xstate_statemachine.eda`, `contrib.brokers` |
//! | `patterns` | `xstate-patterns` | `xstate_statemachine.patterns` |
//! | `observability` | `xstate-observability` | `contrib.observability` (`PrometheusPlugin`) |
//! | `inspect` | `xstate-inspect` | `xstate_statemachine.inspect` |
//! | `macros` | `xstate-macros` | `xstate_statemachine.pythonic` |
//! | `axum` | `xstate-axum` | `contrib.starlette`, `contrib.fastapi` |
//! | `agents` | `xstate-agents` | `contrib.agents` |
//! | `testing` | `xstate-testing` | `testing_utils`, `contrib.testing` |
//!
//! Items of [`core`] are also re-exported at the crate root, so `xstate_statemachine::Machine`
//! works. The command-line tool (`xsm`) and the Python bindings are separate packages.
//!
//! [`core`]: crate::core

#![forbid(unsafe_code)]

pub use xstate_core::*;

/// The engine: machine model, JSON parser, step engine, synchronous interpreter, snapshots.
pub use xstate_core as core;

/// The async interpreter on tokio: mailbox, timers, invoked services, actors.
#[cfg(feature = "tokio")]
pub use xstate_tokio as tokio;

/// Stores, locks, the `persisted` transaction wrapper, durable timers, inbox and log.
#[cfg(feature = "persistence")]
pub use xstate_persistence as persistence;

/// CloudEvents envelopes, the broker contract, dispatcher, outbox, dead letters and AsyncAPI.
#[cfg(feature = "eda")]
pub use xstate_eda as eda;

/// Retry policy, circuit breaker, saga builder and choreography router.
#[cfg(feature = "patterns")]
pub use xstate_patterns as patterns;

/// Metrics through the `metrics` facade.
#[cfg(feature = "observability")]
pub use xstate_observability as observability;

/// Stately Inspector protocol, JSONL recording and replay, SSE sink.
#[cfg(feature = "inspect")]
pub use xstate_inspect as inspect;

/// Typed events, typed states and the `statechart!` DSL.
#[cfg(feature = "macros")]
pub use xstate_macros as macros;

/// axum integration: registry, REST, SSE and WebSocket routes.
#[cfg(feature = "axum")]
pub use xstate_axum as axum;

/// LLM agents as statecharts.
#[cfg(feature = "agents")]
pub use xstate_agents as agents;

/// Test helpers.
#[cfg(feature = "testing")]
pub use xstate_testing as testing;

/// Plugins and observer lifecycle extensions.
pub mod plugins {
    pub use xstate_core::{Observer, Plugin, TracingObserver, TracingPlugin};

    #[cfg(feature = "inspect")]
    pub use xstate_inspect::InspectorObserver as InspectorPlugin;

    #[cfg(feature = "observability")]
    pub use xstate_observability::MetricsObserver as MetricsPlugin;

    #[cfg(feature = "persistence")]
    pub use xstate_persistence::TransitionLogObserver as AuditLogPlugin;

    #[cfg(feature = "agents")]
    pub use xstate_agents::BudgetPlugin;

    #[cfg(feature = "patterns")]
    pub use xstate_patterns::{
        DiagnosticPlugin, DrainPermit, DrainPlugin, DrainingError, FenceOutcome, FlightStep,
        IdempotencyGuard, IdempotencyPlugin, IdempotencyRecord, IdempotencyStatus,
        RateLimiterPlugin, SequenceFencePlugin, WATCHDOG_TIMEOUT_EVENT, WatchdogAlert,
        WatchdogPlugin,
    };
}

/// The everyday items. `use xstate_statemachine::prelude::*;` is enough for most programs.
///
/// Always available: `Machine`, `MachineLogic`, `SyncInterpreter`, `Event`, `Snapshot`, `Observer`,
/// `Plugin`, `TracingPlugin` and the `TypedEvent` / `TypedState` traits. Features add `Interpreter`
/// and `spawn` (`tokio`), `persisted` and `StateStore` (`persistence`), `InspectorObserver` /
/// `InspectorPlugin` (`inspect`), `MetricsObserver` / `MetricsPlugin` (`observability`) and the
/// `TypedEvent` / `TypedState` derives and the `statechart!` macro (`macros`).
pub mod prelude {
    pub use xstate_core::{
        Event, Machine, MachineLogic, Observer, Plugin, Snapshot, SyncInterpreter, TracingObserver,
        TracingPlugin, TypedEvent, TypedState,
    };

    #[cfg(feature = "tokio")]
    pub use xstate_tokio::{Interpreter, spawn};

    #[cfg(feature = "persistence")]
    pub use xstate_persistence::TransitionLogObserver as AuditLogPlugin;
    #[cfg(feature = "persistence")]
    pub use xstate_persistence::{AuditKeys, StateStore, TransitionLogObserver, persisted, redact};

    #[cfg(feature = "inspect")]
    pub use xstate_inspect::InspectorObserver;
    #[cfg(feature = "inspect")]
    pub use xstate_inspect::InspectorObserver as InspectorPlugin;

    #[cfg(feature = "observability")]
    pub use xstate_observability::MetricsObserver;
    #[cfg(feature = "observability")]
    pub use xstate_observability::MetricsObserver as MetricsPlugin;

    #[cfg(feature = "agents")]
    pub use xstate_agents::BudgetPlugin;

    #[cfg(feature = "patterns")]
    pub use xstate_patterns::{
        DiagnosticPlugin, DrainPlugin, IdempotencyPlugin, RateLimiterPlugin, SequenceFencePlugin,
        WatchdogPlugin,
    };

    // The derives live in the macro namespace, the traits above in the type namespace, so the
    // same names serve both (the way `serde::Serialize` does).
    #[cfg(feature = "macros")]
    pub use xstate_macros::{TypedEvent, TypedState, statechart};
}
