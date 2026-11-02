//! `xstate_rs.MachineLogic`: Python callables registered as the engine's actions, guards,
//! services and delays.
//!
//! Each callable is wrapped in a closure the core [`MachineLogic`] can call. The closure
//! attaches to the interpreter (a no-op when the calling thread already holds it), converts the
//! context and event to fresh Python objects, calls the function and converts the result back.
//! A Python exception never unwinds through Rust: it becomes an [`xstate_core::Error::Logic`]
//! carrying the exception text, the machine's `actionErrorPolicy` / `guardErrorPolicy` decide
//! what happens next, and the original exception is parked in an [`ErrorSlot`] so the
//! interpreter can chain it as `__cause__` (or put it on the receipt).

use std::borrow::Cow;
use std::sync::{Arc, Mutex, PoisonError};

use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt};
use pyo3::{PyTraverseError, PyVisit};
use serde_json::Value;
use xstate_core::{ActionScope, Context as _, Event, MachineLogic};

use crate::convert::{self, type_name};

type CoreResult<T> = xstate_core::Result<T>;

/// The first Python exception raised by a callback during one interpreter operation.
#[derive(Default)]
pub(crate) struct ErrorSlot {
    first: Mutex<Option<PyErr>>,
}

impl ErrorSlot {
    /// Forget any parked exception (start of an operation).
    pub(crate) fn clear(&self) {
        *self.first.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Take the parked exception, if any.
    pub(crate) fn take(&self) -> Option<PyErr> {
        self.first.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn record(&self, err: PyErr) {
        let mut slot = self.first.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(err);
        }
    }
}

/// Convert a callback failure into an engine error. `record` parks the exception for the
/// caller; a `BaseException` that is not an `Exception` (`KeyboardInterrupt`, `SystemExit`) is
/// always parked so the interpreter can re-raise it even when a policy swallowed the error.
fn fail(py: Python<'_>, slot: &ErrorSlot, err: PyErr, record: bool) -> xstate_core::Error {
    let message = exception_text(py, &err);
    if record || !err.is_instance_of::<PyException>(py) {
        slot.record(err);
    }
    xstate_core::Error::Logic(message)
}

/// `str(exc)`, or the exception's type name when that is empty.
pub(crate) fn exception_text(py: Python<'_>, err: &PyErr) -> String {
    let value = err.value(py);
    let text = value.str().and_then(|s| s.to_cow().map(Cow::into_owned)).unwrap_or_default();
    if text.is_empty() { type_name(value.as_any()) } else { text }
}

enum DelaySpec {
    Fixed(f64),
    Callable(Py<PyAny>),
}

/// Registry of Python implementations, mirroring the Python library's `MachineLogic`.
#[pyclass(name = "MachineLogic", module = "xstate_rs", frozen)]
pub(crate) struct PyMachineLogic {
    actions: Vec<(String, Py<PyAny>)>,
    guards: Vec<(String, Py<PyAny>)>,
    services: Vec<(String, Py<PyAny>)>,
    delays: Vec<(String, DelaySpec)>,
}

#[pymethods]
impl PyMachineLogic {
    #[new]
    #[pyo3(signature = (actions = None, guards = None, services = None, delays = None))]
    fn new(
        actions: Option<&Bound<'_, PyAny>>,
        guards: Option<&Bound<'_, PyAny>>,
        services: Option<&Bound<'_, PyAny>>,
        delays: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let mut delay_specs = Vec::new();
        for (name, value) in mapping_items(delays, "delays")? {
            delay_specs.push((name, delay_spec(&value)?));
        }
        Ok(Self {
            actions: callables(actions, "actions")?,
            guards: callables(guards, "guards")?,
            services: callables(services, "services")?,
            delays: delay_specs,
        })
    }

    /// Registered actions, `{name: callable}`.
    #[getter]
    fn actions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        to_dict(py, self.actions.iter().map(|(k, v)| (k, v.bind(py).clone())))
    }

    /// Registered guards, `{name: callable}`.
    #[getter]
    fn guards<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        to_dict(py, self.guards.iter().map(|(k, v)| (k, v.bind(py).clone())))
    }

    /// Registered services, `{name: callable}`.
    #[getter]
    fn services<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        to_dict(py, self.services.iter().map(|(k, v)| (k, v.bind(py).clone())))
    }

    /// Registered delays, `{name: milliseconds | callable}`.
    #[getter]
    fn delays<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let items = self.delays.iter().map(|(k, v)| {
            let value = match v {
                DelaySpec::Fixed(ms) => PyFloat::new(py, *ms).into_any(),
                DelaySpec::Callable(f) => f.bind(py).clone(),
            };
            (k, value)
        });
        to_dict(py, items)
    }

    fn __repr__(&self) -> String {
        format!(
            "MachineLogic(actions={}, guards={}, services={}, delays={})",
            self.actions.len(),
            self.guards.len(),
            self.services.len(),
            self.delays.len()
        )
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        for f in self.callables() {
            visit.call(f)?;
        }
        Ok(())
    }
}

impl PyMachineLogic {
    /// `true` when nothing is registered (the interpreter can then run detached from Python).
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty()
            && self.guards.is_empty()
            && self.services.is_empty()
            && self.delays.is_empty()
    }

    /// Every registered Python callable (for garbage-collector traversal).
    pub(crate) fn callables(&self) -> impl Iterator<Item = &Py<PyAny>> {
        let delays = self.delays.iter().filter_map(|(_, d)| match d {
            DelaySpec::Callable(f) => Some(f),
            DelaySpec::Fixed(_) => None,
        });
        self.actions.iter().chain(&self.guards).chain(&self.services).map(|(_, f)| f).chain(delays)
    }

    /// Build the engine registry; callback failures are parked in `slot`.
    pub(crate) fn build(&self, py: Python<'_>, slot: &Arc<ErrorSlot>) -> MachineLogic<Value> {
        let mut logic = MachineLogic::dynamic();
        for (name, f) in &self.actions {
            logic = logic.action(name.clone(), action_fn(f.clone_ref(py), Arc::clone(slot)));
        }
        for (name, f) in &self.guards {
            logic = logic.guard(name.clone(), guard_fn(f.clone_ref(py), Arc::clone(slot)));
        }
        for (name, f) in &self.services {
            logic = logic.service(name.clone(), service_fn(f.clone_ref(py), Arc::clone(slot)));
        }
        for (name, spec) in &self.delays {
            logic = match spec {
                DelaySpec::Fixed(ms) => logic.delay(name.clone(), *ms),
                DelaySpec::Callable(f) => {
                    logic.delay_fn(name.clone(), delay_fn(f.clone_ref(py), Arc::clone(slot)))
                }
            };
        }
        logic
    }
}

fn to_dict<'py, 'a>(
    py: Python<'py>,
    items: impl Iterator<Item = (&'a String, Bound<'py, PyAny>)>,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (k, v) in items {
        dict.set_item(k, v)?;
    }
    Ok(dict)
}

/// `(name, value)` pairs of a mapping argument (`dict` or any object with `.items()`).
fn mapping_items<'py>(
    mapping: Option<&Bound<'py, PyAny>>,
    what: &str,
) -> PyResult<Vec<(String, Bound<'py, PyAny>)>> {
    let Some(mapping) = mapping.filter(|m| !m.is_none()) else { return Ok(Vec::new()) };
    let items = mapping
        .call_method0("items")
        .map_err(|_| PyTypeError::new_err(format!("{what} must be a mapping of name -> value")))?;
    let mut out = Vec::new();
    for item in items.try_iter()? {
        let (name, value): (String, Bound<'py, PyAny>) = item?.extract()?;
        out.push((name, value));
    }
    Ok(out)
}

fn callables(mapping: Option<&Bound<'_, PyAny>>, what: &str) -> PyResult<Vec<(String, Py<PyAny>)>> {
    let mut out = Vec::new();
    for (name, value) in mapping_items(mapping, what)? {
        if !value.is_callable() {
            return Err(PyTypeError::new_err(format!(
                "{what}['{name}'] must be callable, not '{}'",
                type_name(&value)
            )));
        }
        out.push((name, value.unbind()));
    }
    Ok(out)
}

fn delay_spec(value: &Bound<'_, PyAny>) -> PyResult<DelaySpec> {
    let is_number = value.cast::<PyBool>().is_err()
        && (value.cast::<PyInt>().is_ok() || value.cast::<PyFloat>().is_ok());
    if is_number {
        let ms: f64 = value.extract()?;
        if !ms.is_finite() || ms < 0.0 {
            return Err(PyValueError::new_err(format!("delay must be >= 0 ms, got {ms}")));
        }
        return Ok(DelaySpec::Fixed(ms));
    }
    if value.is_callable() {
        return Ok(DelaySpec::Callable(value.clone().unbind()));
    }
    Err(PyTypeError::new_err(format!(
        "a delay must be milliseconds or a callable, not '{}'",
        type_name(value)
    )))
}

fn action_fn(
    f: Py<PyAny>,
    slot: Arc<ErrorSlot>,
) -> impl Fn(&mut ActionScope<'_, Value>) -> CoreResult<()> + Send + Sync + 'static {
    move |scope| {
        Python::attach(|py| {
            call_action(py, f.bind(py), scope).map_err(|e| fail(py, &slot, e, true))
        })
    }
}

/// `f(context, event, params)`; the (possibly mutated) context dict is written back, then a
/// returned dict is merged in as an `assign` patch.
fn call_action(
    py: Python<'_>,
    f: &Bound<'_, PyAny>,
    scope: &mut ActionScope<'_, Value>,
) -> PyResult<()> {
    let ctx = convert::to_py(py, scope.context)?;
    let event = convert::event_to_py(py, scope.event)?;
    let params = convert::to_py(py, scope.params)?;
    let returned = f.call1((&ctx, event, params))?;
    let mut updated = convert::from_py(&ctx)?;
    if !returned.is_none() {
        let patch = returned.cast::<PyDict>().map_err(|_| {
            PyTypeError::new_err(format!(
                "an action must return None or a dict patch, not '{}'",
                type_name(&returned)
            ))
        })?;
        let patch = convert::dict_to_map(patch)?;
        updated.assign_json(&patch).map_err(|e| PyValueError::new_err(e.to_string()))?;
    }
    *scope.context = updated;
    Ok(())
}

fn guard_fn(
    f: Py<PyAny>,
    slot: Arc<ErrorSlot>,
) -> impl Fn(&Value, &Event, &Value) -> CoreResult<bool> + Send + Sync + 'static {
    move |ctx, event, params| {
        Python::attach(|py| {
            let call = || -> PyResult<bool> {
                let args = (
                    convert::to_py(py, ctx)?,
                    convert::event_to_py(py, event)?,
                    convert::to_py(py, params)?,
                );
                f.bind(py).call1(args)?.is_truthy()
            };
            call().map_err(|e| fail(py, &slot, e, true))
        })
    }
}

/// Services complete synchronously. A raised `Exception` is the service's failure (it becomes
/// `xstate.error.actor.<id>`), so it is not reported on the receipt.
fn service_fn(
    f: Py<PyAny>,
    slot: Arc<ErrorSlot>,
) -> impl Fn(&Value, &Value) -> CoreResult<Value> + Send + Sync + 'static {
    move |ctx, input| {
        Python::attach(|py| {
            let call = || -> PyResult<Value> {
                let out =
                    f.bind(py).call1((convert::to_py(py, ctx)?, convert::to_py(py, input)?))?;
                convert::from_py(&out)
            };
            call().map_err(|e| fail(py, &slot, e, false))
        })
    }
}

fn delay_fn(
    f: Py<PyAny>,
    slot: Arc<ErrorSlot>,
) -> impl Fn(&Value, &Event) -> CoreResult<f64> + Send + Sync + 'static {
    move |ctx, event| {
        Python::attach(|py| {
            let call = || -> PyResult<f64> {
                let out = f
                    .bind(py)
                    .call1((convert::to_py(py, ctx)?, convert::event_to_py(py, event)?))?;
                out.extract::<f64>()
            };
            call().map_err(|e| fail(py, &slot, e, true))
        })
    }
}
