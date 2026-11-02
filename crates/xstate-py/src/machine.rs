//! `xstate_rs.Machine`: a compiled, immutable statechart.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};
use xstate_core::Machine;

use crate::convert;
use crate::errors;
use crate::logic::PyMachineLogic;

/// A compiled statechart, the counterpart of the Python library's `MachineNode`. Immutable and
/// cheap to share between interpreters.
#[pyclass(name = "Machine", module = "xstate_rs", frozen)]
pub(crate) struct PyMachine {
    pub(crate) inner: Machine,
}

impl PyMachine {
    pub(crate) fn wrap(inner: Machine) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyMachine {
    /// Parse and compile an XState JSON document.
    #[staticmethod]
    fn from_json(py: Python<'_>, text: &Bound<'_, PyString>) -> PyResult<Self> {
        Machine::from_json(&text.to_cow()?)
            .map(Self::wrap)
            .map_err(|e| errors::config_error(py, &e))
    }

    /// Compile an XState configuration given as a Python `dict`.
    #[staticmethod]
    fn from_dict(config: &Bound<'_, PyDict>) -> PyResult<Self> {
        let value = serde_json::Value::Object(convert::dict_to_map(config)?);
        Machine::from_value(&value)
            .map(Self::wrap)
            .map_err(|e| errors::config_error(config.py(), &e))
    }

    /// Machine id (the root key).
    #[getter]
    fn id(&self) -> &str {
        self.inner.id()
    }

    /// Chart `version` label, if any.
    #[getter]
    fn version(&self) -> Option<&str> {
        self.inner.version()
    }

    /// The Python-compatible structure hash (16 hex characters).
    #[getter]
    fn structure_hash(&self) -> &str {
        self.inner.structure_hash()
    }

    /// Event types any state declares (exact descriptors), sorted.
    #[getter]
    fn declared_events(&self) -> Vec<String> {
        self.inner.declared_events().to_vec()
    }

    /// Every state id (root first), in document order.
    #[getter]
    fn state_ids(&self) -> Vec<String> {
        self.inner.states().iter().map(|s| s.id.to_string()).collect()
    }

    /// The configuration document as pretty-printed JSON text.
    fn to_json(&self) -> String {
        self.inner.to_json_string()
    }

    /// The configuration document as a Python `dict`.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_py(py, &self.inner.to_json())
    }

    /// Mermaid `stateDiagram-v2` rendering.
    fn to_mermaid(&self) -> String {
        self.inner.to_mermaid()
    }

    /// `PlantUML` rendering.
    fn to_plantuml(&self) -> String {
        self.inner.to_plantuml()
    }

    /// Check the machine (and, when given, that `logic` implements every action, guard,
    /// service and delay it names). Returns the parse-time warnings as `"path: message"`.
    #[pyo3(signature = (logic = None))]
    fn validate(
        &self,
        py: Python<'_>,
        logic: Option<&Bound<'_, PyMachineLogic>>,
    ) -> PyResult<Vec<String>> {
        if let Some(logic) = logic {
            let core = logic.get().build(py, &std::sync::Arc::default());
            core.validate(&self.inner).map_err(|e| errors::to_py_err(py, &e))?;
        }
        Ok(self.inner.warnings().iter().map(|w| format!("{}: {}", w.path, w.message)).collect())
    }

    fn __repr__(&self) -> String {
        format!(
            "Machine(id={:?}, states={}, structure_hash={:?})",
            self.inner.id(),
            self.inner.states().len(),
            self.inner.structure_hash()
        )
    }
}
