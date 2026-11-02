# xstate-observability

Metrics for `xstate-core` machines through the [`metrics`](https://docs.rs/metrics) facade.
`MetricsObserver` records the metric set of the Python library's `PrometheusPlugin` (same
`xstatemachine_*` names and labels where the hooks expose the data). The application picks the
exporter, for example `metrics-exporter-prometheus`; this crate does not depend on one.

Label values obey strict telemetry hygiene: event types must be declared by the chart (otherwise
`unknown`), every label dimension is capped (overflow is `other`), and payloads, context and
instance keys are never recorded.

Status: 0.1.0. Transition durations are recorded through `MetricsObserver::timed` because the
observer hooks fire after the fact.

```rust
use std::sync::Arc;
use xstate_core::{Machine, MachineLogic, SyncInterpreter};
use xstate_observability::{MetricsObserver, describe_metrics};

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

## Features

None.

## Python counterpart

[`xstate_statemachine.contrib.observability`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/contrib/observability)
(`PrometheusPlugin`). OpenTelemetry and structlog / sentry plugins have no Rust counterpart yet;
the engine already emits `tracing` events (`TracingObserver` in `xstate-core`).
