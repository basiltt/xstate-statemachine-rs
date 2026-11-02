//! `xstate_rs.Receipt`: the outcome of `start` / `send` / `tick`.

use pyo3::prelude::*;
use pyo3::types::{PyFrozenSet, PyList, PyString};
use xstate_core::Receipt;

/// What processing one event (and everything it triggered) did, mirroring the Python
/// library's `Receipt`. Field values are computed lazily from the engine receipt, so an
/// unread receipt costs almost nothing.
#[pyclass(name = "Receipt", module = "xstate_rs", frozen)]
pub(crate) struct PyReceipt {
    inner: Receipt,
    error: Option<Py<PyAny>>,
}

impl PyReceipt {
    pub(crate) fn new(inner: Receipt, error: Option<Py<PyAny>>) -> Self {
        Self { inner, error }
    }
}

#[pymethods]
impl PyReceipt {
    /// Active leaf state ids after processing (a `frozenset`, as in the Python library).
    #[getter]
    fn state_ids<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyFrozenSet>> {
        PyFrozenSet::new(py, self.inner.state_ids())
    }

    /// Whether the event changed the configuration or the context.
    #[getter]
    fn changed(&self) -> bool {
        self.inner.changed
    }

    /// Whether no transition matched the event.
    #[getter]
    fn unhandled(&self) -> bool {
        self.inner.unhandled
    }

    /// User actions executed while processing, in order.
    #[getter]
    fn actions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.inner.actions.iter().map(|a| &**a))
    }

    /// Interpreter status after processing (`"running"`, `"done"`, `"error"`, `"stopped"`).
    #[getter]
    fn status(&self) -> &'static str {
        self.inner.status.as_str()
    }

    /// The first exception a Python callback raised while processing, when a policy kept the
    /// machine going (`actionErrorPolicy` `continue` / `rollback`, `guardErrorPolicy`), or
    /// `None` when everything ran cleanly.
    #[getter]
    fn error(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.error.as_ref().map(|e| e.clone_ref(py))
    }

    /// `True` when the state with this id is active after processing.
    fn matches(&self, state_id: &Bound<'_, PyString>) -> PyResult<bool> {
        Ok(self.inner.matches(&state_id.to_cow()?))
    }

    fn __repr__(&self) -> String {
        format!(
            "Receipt(state_ids={:?}, changed={}, unhandled={}, actions={:?}, status={:?}, error={})",
            self.inner.state_ids(),
            py_bool(self.inner.changed),
            py_bool(self.inner.unhandled),
            self.inner.action_names(),
            self.inner.status.as_str(),
            if self.error.is_some() { "<exception>" } else { "None" }
        )
    }
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}
