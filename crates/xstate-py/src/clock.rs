//! `xstate_rs.SimulatedClock`: virtual time for deterministic timer tests.

use std::sync::{Mutex, PoisonError};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyWeakrefReference;
use xstate_core::{Clock as _, SimulatedClock};

use crate::interpreter::PySyncInterpreter;

/// Virtual time that only moves when told to, mirroring the Python library's
/// `SimulatedClock`. `increment(ms)` advances it and fires what became due on every started
/// interpreter that uses this clock, in due order.
#[pyclass(name = "SimulatedClock", module = "xstate_rs", frozen)]
pub(crate) struct PySimulatedClock {
    pub(crate) inner: SimulatedClock,
    attached: Mutex<Vec<Py<PyWeakrefReference>>>,
}

#[pymethods]
impl PySimulatedClock {
    #[new]
    fn new() -> Self {
        Self { inner: SimulatedClock::new(), attached: Mutex::new(Vec::new()) }
    }

    /// Virtual time in milliseconds.
    #[getter]
    fn now_ms(&self) -> f64 {
        self.inner.now_ms()
    }

    /// Virtual time in seconds (the Python library's `Clock.now()`).
    fn now(&self) -> f64 {
        self.inner.now_ms() / 1000.0
    }

    /// Advance by `ms` and fire every timer that became due on the attached interpreters.
    fn increment(&self, py: Python<'_>, ms: f64) -> PyResult<()> {
        if !ms.is_finite() || ms < 0.0 {
            return Err(PyValueError::new_err("SimulatedClock cannot move backwards"));
        }
        self.inner.increment(ms);
        self.settle(py)
    }

    /// Jump to the absolute virtual time `ms` and fire what became due.
    fn set(&self, py: Python<'_>, ms: f64) -> PyResult<()> {
        let now = self.inner.now_ms();
        if !ms.is_finite() || ms < now {
            return Err(PyValueError::new_err(format!(
                "SimulatedClock.set({ms}) would move backwards from {now} ms"
            )));
        }
        self.inner.set(ms);
        self.settle(py)
    }

    fn __repr__(&self) -> String {
        format!("SimulatedClock(now_ms={})", self.inner.now_ms())
    }
}

impl PySimulatedClock {
    /// Remember `interp` (weakly) so `increment` can fire its timers.
    pub(crate) fn attach(&self, interp: &Bound<'_, PyAny>) -> PyResult<()> {
        let weak = PyWeakrefReference::new(interp)?.unbind();
        self.attached.lock().unwrap_or_else(PoisonError::into_inner).push(weak);
        Ok(())
    }

    /// Tick every live attached interpreter; dead references are pruned. The list is copied
    /// before ticking so a callback that creates or drops interpreters cannot deadlock.
    fn settle(&self, py: Python<'_>) -> PyResult<()> {
        let live: Vec<Bound<'_, PySyncInterpreter>> = {
            let mut attached = self.attached.lock().unwrap_or_else(PoisonError::into_inner);
            let mut live = Vec::with_capacity(attached.len());
            attached.retain(|weak| match weak.bind(py).upgrade_as::<PySyncInterpreter>() {
                Ok(Some(interp)) => {
                    live.push(interp);
                    true
                }
                _ => false,
            });
            live
        };
        for interp in live {
            PySyncInterpreter::tick_from_clock(&interp)?;
        }
        Ok(())
    }
}
