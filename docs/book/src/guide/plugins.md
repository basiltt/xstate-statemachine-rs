# Plugins & Observability

Observe every event, microstep, transition, action, guard, and service in your statecharts with zero-allocation, infallible lifecycle hooks.

---

## 🔌 What are Plugins?

```mermaid
flowchart LR
    I["⚙️ SyncInterpreter / Tokio Actor"] -- on_event --> T["🪵 TracingPlugin (zero-alloc)"]
    I -- on_event --> M["📈 MetricsPlugin (Prometheus)"]
    I -- on_event --> S["🕵️ InspectorPlugin (Stately SSE)"]
    I -- on_event --> A["📜 AuditLogPlugin (SQL / JSONL)"]
    T -. "observe only — never mutates" .-> I
```

In `xstate-statemachine`, a **Plugin** implements the **Observer pattern**: it subscribes to lifecycle events across the interpreter and reacts to them (e.g. logging structured spans, incrementing Prometheus counters, streaming to Stately Studio, or writing tamper-proof audit trails).

### Key Architectural Guarantees

1. **Non-Invasive:** Plugins observe statechart execution; they cannot alter machine transitions, mutate state topology, or interfere with fast-path decision logic.
2. **Zero Overhead Fast Path:** Attaching zero plugins incurs 0 runtime cost (`is_empty` check). When attached, hooks receive borrowed references (`&Machine`, `&Event`, `&ActiveState<C>`) without performing heap allocations.
3. **Panic Containment:** In Tokio actors, plugin panics are caught and contained; a faulty plugin cannot bring down your statechart actor.
4. **Thread-Safe by Design:** Plugins implement `Send + Sync`. Observers maintain state via interior mutability (`AtomicU64`, `Mutex`, `RwLock`).

---

## 🪝 Complete Hook Reference

Every plugin implements the [`Observer<C>`](https://docs.rs/xstate-core/latest/xstate_core/observer/trait.Observer.html) trait. All methods provide default no-op implementations:

| Hook | Signature | When It Fires |
|:---|:---|:---|
| `on_start` | `(&self, machine, state, report)` | When the interpreter initializes and enters its initial configuration |
| `on_event` | `(&self, machine, event, report, state)` | Immediately after an event is processed to completion (macrostep settled) |
| `on_event_dropped` | `(&self, machine, event, reason)` | When an event cannot be processed (`"stopped"`, `"deferred"`, `"deferred_overflow"`, `"unresolved_target"`) |
| `on_error` | `(&self, machine, error)` | When a transition or effect fails with an execution error |
| `on_stop` | `(&self, machine, state)` | When the interpreter or actor is stopped by the host application |

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;

// In xstate-statemachine, Plugin<C> is an alias for Observer<C>
pub struct CustomAuditPlugin;

impl<C: Context> Observer<C> for CustomAuditPlugin {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, report: &StepReport) {
        println!("🚀 Machine '{}' started in state: {:?}", machine.id(), state.leaf_ids(machine));
    }

    fn on_event(&self, machine: &Machine, event: &Event, report: &StepReport, state: &ActiveState<C>) {
        println!(
            "⚡ Machine '{}' processed '{}' -> now in {:?} (changed: {}, actions: {})",
            machine.id(),
            event.ty,
            state.leaf_ids(machine),
            report.changed,
            report.actions.len()
        );
    }

    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {
        eprintln!("⚠️ Event '{}' dropped on machine '{}'. Reason: {reason}", event.ty, machine.id());
    }

    fn on_error(&self, machine: &Machine, error: &xstate_core::Error) {
        eprintln!("🚨 Step failed on machine '{}': {error}", machine.id());
    }

    fn on_stop(&self, machine: &Machine, state: &ActiveState<C>) {
        println!("🛑 Machine '{}' stopped. Final status: {}", machine.id(), state.status().as_str());
    }
}
```

---

## 📦 Built-in Plugins

The workspace ships with production-grade, out-of-the-box plugins:

### 1. `TracingPlugin` (Structured Logging & Diagnostics)

Emits zero-heap-allocation, structured diagnostics via the [`tracing`](https://crates.io/crates/tracing) facade under target `xstate`.

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{"id": "door", "initial": "closed", "states": {"closed": {"on": {"OPEN": "opened"}}, "opened": {}}}"#)?;
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());

// Attach TracingPlugin
interp.plugin(Arc::new(TracingPlugin));
interp.start()?;
interp.send("OPEN")?;
# Ok::<(), xstate_core::Error>(())
```

Emitted structured log output:
```text
INFO xstate: started machine="door" states=["door.closed"]
INFO xstate: event processed machine="door" event="OPEN" changed=true unhandled=false actions=[] states=["door.opened"] status="running"
```

> 🔒 **Sensitive Credential Redaction:** Payloads and sensitive context entries matching `password`, `secret`, `token`, `api_key`, `authorization`, `credit_card` (from `DEFAULT_REDACT_KEYS`) are automatically masked as `"***"`.

---

### 2. `MetricsPlugin` (Prometheus Observability)

Tracks transitions, event frequencies, error rates, and microstep latency histograms via the Rust `metrics` facade.

```toml
[dependencies]
xstate-statemachine = { version = "0.1", features = ["observability"] }
```

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::plugins::MetricsPlugin;

let machine = Machine::from_json(CHART_JSON)?;
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());

// Attach the Prometheus metrics observer
interp.plugin(Arc::new(MetricsPlugin::new()));
interp.start()?;
```

Exposed Prometheus metrics:
* `xstate_transitions_total{machine="order", from="pending", to="processing"}`
* `xstate_events_received_total{machine="order", event="PAYMENT_RECEIVED"}`
* `xstate_step_duration_seconds_bucket{machine="order", le="0.0001"}`
* `xstate_dropped_events_total{machine="order", reason="unresolved_target"}`

---

### 3. `InspectorPlugin` (Stately Studio Live Inspection)

Connects your running Rust statechart directly to [Stately Studio](https://stately.ai/editor) via Server-Sent Events (SSE) or streams deterministic execution traces to JSONL files.

```toml
[dependencies]
xstate-statemachine = { version = "0.1", features = ["inspect"] }
```

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::inspect::{InspectorObserver, SseSink};

let sink = Arc::new(SseSink::new(100));
let inspector = Arc::new(InspectorObserver::with_shared_sink(sink.clone()));

let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.plugin(inspector);
interp.start()?;
```

---

### 4. `AuditLogPlugin` (Tamper-Proof Transition Journal)

Records immutable, serializable [`TransitionRecord`](https://docs.rs/xstate-persistence/latest/xstate_persistence/log/struct.TransitionRecord.html) audit events to memory, SQLite, Postgres, or Redis.

```toml
[dependencies]
xstate-statemachine = { version = "0.1", features = ["persistence", "sqlite"] }
```

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::persistence::{SqliteLog, TransitionLogObserver};

let log_store = Arc::new(SqliteLog::open("audit_trail.db")?);
let audit_plugin = Arc::new(TransitionLogObserver::new(log_store));

let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.plugin(audit_plugin);
interp.start()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

---

### 5. `BudgetPlugin` (LLM Autonomous Agent Safety)

Enforces strict step limits, token expenditure caps, dollar budgets, and state-level tool allow-lists for autonomous LLM agents.

```toml
[dependencies]
xstate-statemachine = { version = "0.1", features = ["agents"] }
```

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::agents::BudgetPlugin;

let budget = Arc::new(BudgetPlugin::new()
    .max_steps(25)
    .max_tokens(50_000)
    .max_cost_usd(0.50));

let mut interp = SyncInterpreter::new(agent_machine, logic);
interp.plugin(budget);
interp.start()?;
```

---

## 🛠️ Writing a Custom Plugin: Step-by-Step

Here is a complete, production-grade example of an **Audit Trail & Latency Monitor Plugin** that records transition history, timing, and unhandled event alerts:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use parking_lot::Mutex;
use xstate_statemachine::prelude::*;

#[derive(Debug)]
pub struct TransitionEntry {
    pub timestamp: Instant,
    pub event: String,
    pub states: Vec<String>,
    pub changed: bool,
}

pub struct AuditMonitorPlugin {
    total_events: AtomicU64,
    unhandled_events: AtomicU64,
    history: Mutex<Vec<TransitionEntry>>,
}

impl AuditMonitorPlugin {
    pub fn new() -> Self {
        Self {
            total_events: AtomicU64::new(0),
            unhandled_events: AtomicU64::new(0),
            history: Mutex::new(Vec::with_capacity(128)),
        }
    }

    pub fn total_events(&self) -> u64 {
        self.total_events.load(Ordering::Relaxed)
    }

    pub fn unhandled_events(&self) -> u64 {
        self.unhandled_events.load(Ordering::Relaxed)
    }

    pub fn recent_transitions(&self) -> Vec<TransitionEntry> {
        self.history.lock().clone()
    }
}

impl<C: Context> Observer<C> for AuditMonitorPlugin {
    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        self.total_events.fetch_add(1, Ordering::Relaxed);
        if report.unhandled {
            self.unhandled_events.fetch_add(1, Ordering::Relaxed);
        }

        let entry = TransitionEntry {
            timestamp: Instant::now(),
            event: event.ty.to_string(),
            states: state.leaf_ids(machine).iter().map(|s| s.to_string()).collect(),
            changed: report.changed,
        };

        let mut hist = self.history.lock();
        if hist.len() >= 1000 {
            hist.remove(0); // Bounded ring-buffer retention
        }
        hist.push(entry);
    }

    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {
        self.unhandled_events.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "audit",
            machine = machine.id(),
            event = %event.ty,
            reason,
            "Event dropped without execution"
        );
    }
}
```

### Usage with Sync Interpreter & Tokio Actors

```rust
// 1. With Synchronous Interpreter
let plugin = Arc::new(AuditMonitorPlugin::new());
let mut interp = SyncInterpreter::new(machine, logic);
interp.plugin(plugin.clone());
interp.start()?;

// 2. With Tokio Actor
let actor = xstate_tokio::spawn_with_observers(
    machine,
    Arc::new(logic),
    xstate_tokio::Options::default(),
    vec![plugin.clone()],
)?;
```

---

## ⚡ Error Containment & Infallibility Rules

When authoring custom plugins, adhere to these operational invariants:

1. **Infallible Callbacks:** Hook signatures return `()` by design. A plugin must never raise an unhandled exception or abort the machine's execution.
2. **Never Call `send()` Mid-Step:** Calling `interp.send()` from within `on_event` or `on_start` triggers recursive re-entrancy. If an observer needs to dispatch an event, defer it to a channel or spawn a Tokio task.
3. **Bounded Memory:** If caching transition records, bound your buffer using a ring buffer or FIFO queue to prevent memory leaks in high-throughput services.
