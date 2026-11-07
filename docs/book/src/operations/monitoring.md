# Monitoring

Observability is split between the `metrics` facade and `tracing`.

## Metrics

`xstate-observability` (feature `observability`) records the `xstatemachine_*` metric set of the
Python library's `PrometheusPlugin` through the
[`metrics`](https://docs.rs/metrics) facade. The crate does not pick the exporter — install one
(e.g. `metrics-exporter-prometheus`) and call `describe_metrics()`.

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::observability::{MetricsObserver, describe_metrics};

describe_metrics(); // after installing an exporter
let machine = Machine::from_json(
    r#"{"id":"door","initial":"closed","states":{
        "closed":{"on":{"OPEN":"open"}},"open":{"on":{"CLOSE":"closed"}}}}"#,
)?;
let observer = Arc::new(MetricsObserver::new());
let mut interpreter = SyncInterpreter::new(machine, MachineLogic::dynamic());
interpreter.observe(observer.clone());
interpreter.start()?;
interpreter.send("OPEN")?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The recorded names include `xstatemachine_transitions`, `xstatemachine_events_received`,
`xstatemachine_transition_duration_seconds`, `xstatemachine_active_interpreters`,
`xstatemachine_chain_trips`, `xstatemachine_actions`, `xstatemachine_unhandled_events`,
`xstatemachine_events_dropped`, `xstatemachine_errors` and `xstatemachine_microsteps` (the
`metrics` facade appends `_total` to counters). Labels obey strict telemetry hygiene: undeclared
event types are recorded as `unknown`, every label dimension is capped (`other` on overflow), and
payloads, context and instance keys are never recorded. Transition durations are measured through
`MetricsObserver::timed`, because the observer hooks fire after the fact.

## Tracing

Library crates use `tracing` only (no `println!`/`log`), with targets like `xstate`,
`xstate::persistence`, `xstate::inspect`, `xstate::eda` and `xstate::observability`. No payload,
context or secret is logged by default; the engine's `redact` helpers and the inspector
allow-lists keep user data out of logs and streams. `TracingObserver` (in `xstate-core`) bridges
interpreter hooks to `tracing`.

See the crate docs under `/api/xstate_observability/` for the full metric catalogue.
