//! Metrics for `xstate-core` machines through the [`metrics`] facade.
//!
//! [`MetricsObserver`] is an [`Observer`](xstate_core::Observer) recording the metric set of the
//! Python library's `PrometheusPlugin` (same names and labels where the hooks expose the
//! data; see [`metric_names`] for the table). The crate records through the `metrics` facade
//! only: the application picks the exporter, for example `metrics-exporter-prometheus`, which
//! this crate does not depend on.
//!
//! Label values obey the telemetry hygiene rules of [`LabelGuard`]: event types must be declared
//! by the chart (otherwise `unknown`), every label dimension is capped (overflow is `other`),
//! and payloads, context and instance keys are never recorded.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use xstate_core::{Machine, MachineLogic, SyncInterpreter};
//! use xstate_observability::{MetricsObserver, describe_metrics};
//!
//! // Application start-up: install an exporter, then register help texts.
//! //
//! //     metrics_exporter_prometheus::PrometheusBuilder::new().install()?;
//! //
//! // (`metrics-exporter-prometheus` is the application's dependency, not this crate's.)
//! describe_metrics();
//!
//! let machine = Machine::from_json(
//!     r#"{"id":"door","initial":"closed","states":{
//!         "closed":{"on":{"OPEN":"open"}},
//!         "open":{"on":{"CLOSE":"closed"}}}}"#,
//! )?;
//! let observer = Arc::new(MetricsObserver::new());
//! let mut interpreter = SyncInterpreter::new(machine, MachineLogic::dynamic());
//! interpreter.observe(observer.clone());
//! interpreter.start()?;
//!
//! // `timed` is the only way to record transition durations: the hooks fire after the fact.
//! let machine = interpreter.machine().clone();
//! observer.timed(&machine, || interpreter.send("OPEN"))?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]

mod hygiene;
pub mod metric_names;
mod metrics_observer;

pub use hygiene::{ALWAYS, DEFAULT_MAX_LABEL_VALUES, LabelGuard, OTHER, UNKNOWN, event_label};
pub use metric_names::describe_metrics;
pub use metrics_observer::MetricsObserver;
