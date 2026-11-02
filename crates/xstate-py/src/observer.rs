//! `SyncInterpreter.observe()`: a Python transition callback as an engine observer.

use pyo3::prelude::*;
use serde_json::Value;
use xstate_core::engine::StepReport;
use xstate_core::{ActiveState, Event, Machine, Observer};

use crate::convert;

/// Forwards the start and every processed event to `callback(event, state_ids, changed)`.
/// Observers must never stop the machine, so an exception the callback raises is reported
/// through `sys.unraisablehook` instead of propagating.
pub(crate) struct PyObserver {
    callback: Py<PyAny>,
}

impl PyObserver {
    pub(crate) fn new(callback: Py<PyAny>) -> Self {
        Self { callback }
    }

    fn notify(&self, event: &Event, state_ids: Vec<String>, changed: bool) {
        Python::attach(|py| {
            let callback = self.callback.bind(py);
            let call = || -> PyResult<()> {
                callback.call1((convert::event_to_py(py, event)?, state_ids, changed)).map(drop)
            };
            if let Err(err) = call() {
                err.write_unraisable(py, Some(callback));
            }
        });
    }
}

impl Observer<Value> for PyObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<Value>, _report: &StepReport) {
        self.notify(&Event::init(), state.leaf_ids(machine), true);
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<Value>,
    ) {
        self.notify(event, state.leaf_ids(machine), report.changed);
    }
}
