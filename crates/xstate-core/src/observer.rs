//! Observers: lifecycle hooks on an interpreter (the Python `PluginBase` equivalent).
//!
//! Observers are shared (`Arc<dyn Observer>`) and take `&self`, so an observer that keeps
//! state uses interior mutability. Hooks are infallible by design: an observer must never
//! stop the machine. [`TracingObserver`] logs every event through `tracing`.

use std::fmt;

use crate::context::Context;
use crate::engine::{ActiveState, StepReport};
use crate::error::Error;
use crate::event::Event;
use crate::machine::Machine;

/// Lifecycle hooks. Every method has a no-op default.
#[allow(unused_variables)]
pub trait Observer<C: Context>: Send + Sync {
    /// The machine entered its initial configuration.
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, report: &StepReport) {}

    /// One event (external, timer or service completion) was processed to completion.
    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
    }

    /// An event was dropped without being processed (deferred under `onUnhandled: defer`, or
    /// rejected because the machine was not running). `reason` is a short machine-readable tag.
    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {}

    /// A step failed with an error.
    fn on_error(&self, machine: &Machine, error: &Error) {}

    /// The machine was stopped by the application.
    fn on_stop(&self, machine: &Machine, state: &ActiveState<C>) {}
}

impl<C: Context> fmt::Debug for dyn Observer<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("dyn Observer")
    }
}

/// Logs every lifecycle hook through [`tracing`] (target `xstate`), the counterpart of the
/// Python `LoggingInspector`. Context and payloads are not logged.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingObserver;

impl<C: Context> Observer<C> for TracingObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, _report: &StepReport) {
        tracing::info!(target: "xstate", machine = machine.id(), states = ?state.leaf_ids(machine), "started");
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        tracing::info!(
            target: "xstate",
            machine = machine.id(),
            event = %event.ty,
            changed = report.changed,
            unhandled = report.unhandled,
            actions = ?report.actions,
            states = ?state.leaf_ids(machine),
            status = state.status().as_str(),
            "event processed"
        );
    }

    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {
        tracing::warn!(target: "xstate", machine = machine.id(), event = %event.ty, reason, "event dropped");
    }

    fn on_error(&self, machine: &Machine, error: &Error) {
        tracing::error!(target: "xstate", machine = machine.id(), %error, "step failed");
    }

    fn on_stop(&self, machine: &Machine, state: &ActiveState<C>) {
        tracing::info!(target: "xstate", machine = machine.id(), status = state.status().as_str(), "stopped");
    }
}
