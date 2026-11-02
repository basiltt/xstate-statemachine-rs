//! `xstate_rs.SyncInterpreter`: runs a machine in the Rust engine on the caller's thread.
//!
//! Every operation is processed to completion before it returns, exactly like the Python
//! library's `SyncInterpreter`. When the interpreter has no Python callbacks at all (no logic,
//! no observers) the engine runs detached from the Python interpreter, so other Python threads
//! keep running; otherwise it stays attached and callbacks are called directly.

use std::sync::Arc;

use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedStr;
use pyo3::types::{PyBool, PyDict, PyList, PySet, PyString};
use pyo3::{PyTraverseError, PyVisit};
use serde_json::Value;
use xstate_core::{
    ActiveState, Clock, MachineLogic, RealClock, Receipt, RestartTimers, RestoreOptions, Snapshot,
    SyncInterpreter,
};

use crate::clock::PySimulatedClock;
use crate::convert;
use crate::errors;
use crate::event::{TypeCache, build_event};
use crate::logic::{ErrorSlot, PyMachineLogic};
use crate::machine::PyMachine;
use crate::observer::PyObserver;
use crate::pyjson;
use crate::receipt::PyReceipt;

type CoreResult<T> = xstate_core::Result<T>;

const BUSY: &str = "the interpreter is busy: it cannot be called from inside its own action, \
                    guard, service, delay or observer (use a `raise` action to queue follow-up \
                    events), nor while another thread is using it";

/// Synchronous interpreter backed by the Rust engine; see the module documentation.
#[pyclass(name = "SyncInterpreter", module = "xstate_rs", weakref)]
pub(crate) struct PySyncInterpreter {
    inner: SyncInterpreter<Value>,
    slot: Arc<ErrorSlot>,
    clock: Option<Py<PySimulatedClock>>,
    clock_attached: bool,
    detachable: bool,
    /// Restored from a snapshot and not yet `start()`ed: `start()` resumes instead.
    restored: bool,
    /// Every Python callable the engine holds (logic and observers), for the GC.
    callables: Vec<Py<PyAny>>,
    types: TypeCache,
}

/// Everything an interpreter needs besides the machine.
struct Parts {
    slot: Arc<ErrorSlot>,
    logic: MachineLogic<Value>,
    clock: Option<Py<PySimulatedClock>>,
    clock_arc: Arc<dyn Clock>,
    detachable: bool,
    callables: Vec<Py<PyAny>>,
}

impl Parts {
    fn new(
        py: Python<'_>,
        logic: Option<&PyMachineLogic>,
        clock: Option<Bound<'_, PySimulatedClock>>,
    ) -> Self {
        let slot = Arc::new(ErrorSlot::default());
        let (core, detachable, callables) = match logic {
            Some(l) => {
                (l.build(py, &slot), l.is_empty(), l.callables().map(|f| f.clone_ref(py)).collect())
            }
            None => (MachineLogic::dynamic(), true, Vec::new()),
        };
        let clock_arc: Arc<dyn Clock> = match &clock {
            Some(c) => Arc::new(c.get().inner.clone()),
            None => Arc::new(RealClock::new()),
        };
        Self {
            slot,
            logic: core,
            clock: clock.map(Bound::unbind),
            clock_arc,
            detachable,
            callables,
        }
    }

    fn into_interpreter(self, inner: SyncInterpreter<Value>, restored: bool) -> PySyncInterpreter {
        PySyncInterpreter {
            inner,
            slot: self.slot,
            clock: self.clock,
            clock_attached: false,
            detachable: self.detachable,
            restored,
            callables: self.callables,
            types: TypeCache::default(),
        }
    }
}

#[pymethods]
impl PySyncInterpreter {
    #[new]
    #[pyo3(signature = (machine, logic = None, *, clock = None, input = None))]
    fn new(
        py: Python<'_>,
        machine: &Bound<'_, PyMachine>,
        logic: Option<&Bound<'_, PyMachineLogic>>,
        clock: Option<Bound<'_, PySimulatedClock>>,
        input: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let input = input.map(convert::from_py).transpose()?.unwrap_or(Value::Null);
        let mut parts = Parts::new(py, logic.map(Bound::get), clock);
        let core = std::mem::take(&mut parts.logic);
        let inner = SyncInterpreter::with_clock(
            machine.get().inner.clone(),
            core,
            Arc::clone(&parts.clock_arc),
        )
        .with_input(input);
        Ok(parts.into_interpreter(inner, false))
    }

    /// Rebuild an interpreter from a layout-v4 snapshot (written by either engine). Entry
    /// actions and services do not re-run; `after` timers follow `restart_timers`
    /// (`"resume"`, `"parked"`, `"fire_due"`, `"restart"`, or `True` / `False`).
    #[staticmethod]
    #[pyo3(signature = (machine, logic, json_text, *, restart_timers = None, verify_hash = true, clock = None))]
    fn from_snapshot<'py>(
        py: Python<'py>,
        machine: &Bound<'_, PyMachine>,
        logic: Option<&Bound<'_, PyMachineLogic>>,
        json_text: &Bound<'_, PyString>,
        restart_timers: Option<&Bound<'_, PyAny>>,
        verify_hash: bool,
        clock: Option<Bound<'_, PySimulatedClock>>,
    ) -> PyResult<Bound<'py, Self>> {
        let options = RestoreOptions {
            verify_hash: Some(verify_hash),
            restart_timers: restart_policy(restart_timers)?,
        };
        let snapshot =
            Snapshot::from_json(&json_text.to_cow()?).map_err(|e| errors::to_py_err(py, &e))?;
        let mut parts = Parts::new(py, logic.map(Bound::get), clock);
        let restored = SyncInterpreter::from_snapshot(
            machine.get().inner.clone(),
            std::mem::take(&mut parts.logic),
            &snapshot,
            Arc::clone(&parts.clock_arc),
            &options,
        );
        let parked = parts.slot.take();
        let inner = match restored {
            Ok(inner) => match parked {
                // A `KeyboardInterrupt` raised by a timer fired under "fire_due" propagates.
                Some(err) if !err.is_instance_of::<PyException>(py) => return Err(err),
                _ => inner,
            },
            Err(e) => return Err(errors::with_cause(py, errors::to_py_err(py, &e), parked)),
        };
        let bound = Bound::new(py, parts.into_interpreter(inner, true))?;
        Self::attach_clock(&bound)?;
        Ok(bound)
    }

    /// Enter the initial configuration (running entry actions). On an interpreter restored
    /// from a snapshot, resume it instead: process restored pending events and due timers.
    fn start(slf: &Bound<'_, Self>) -> PyResult<PyReceipt> {
        Self::attach_clock(slf)?;
        let restored = std::mem::replace(&mut Self::borrow(slf)?.restored, false);
        if restored {
            Self::run(slf, SyncInterpreter::tick)
        } else {
            Self::run(slf, SyncInterpreter::start)
        }
    }

    /// Deliver an event and process it to completion. `event` is a type string, an XState
    /// event dict (`{"type": "ADD", "qty": 2}`) or an object with `type` / `payload`
    /// attributes; keyword arguments are added to the payload.
    #[pyo3(signature = (event, /, **payload))]
    fn send(
        slf: &Bound<'_, Self>,
        event: &Bound<'_, PyAny>,
        payload: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyReceipt> {
        let event = build_event(event, payload, &mut Self::borrow(slf)?.types)?;
        Self::run(slf, move |interp| interp.send(event))
    }

    /// Fire every timer that is due according to the clock.
    fn tick(slf: &Bound<'_, Self>) -> PyResult<PyReceipt> {
        Self::run(slf, SyncInterpreter::tick)
    }

    /// Advance this interpreter's `SimulatedClock` by `ms` and fire what became due here.
    fn advance(slf: &Bound<'_, Self>, ms: f64) -> PyResult<PyReceipt> {
        let py = slf.py();
        if !ms.is_finite() || ms < 0.0 {
            return Err(PyValueError::new_err("advance() needs a finite, non-negative ms"));
        }
        let clock = Self::read(slf)?.clock.as_ref().map(|c| c.get().inner.clone());
        let clock = clock.ok_or_else(|| {
            errors::lifecycle_error(
                py,
                "advance() needs an interpreter built with clock=SimulatedClock()",
            )
        })?;
        Self::run(slf, move |interp| interp.advance(&clock, ms))
    }

    /// Stop the machine; pending events and timers are dropped.
    fn stop(slf: &Bound<'_, Self>) -> PyResult<()> {
        Self::borrow(slf)?.inner.stop();
        Ok(())
    }

    /// Register `callback(event: dict, state_ids: list[str], changed: bool)`, called after
    /// the start and after every processed event. Exceptions it raises are reported through
    /// `sys.unraisablehook` and never stop the machine.
    fn observe(slf: &Bound<'_, Self>, callback: &Bound<'_, PyAny>) -> PyResult<()> {
        if !callback.is_callable() {
            return Err(PyTypeError::new_err("observe() needs a callable"));
        }
        let mut this = Self::borrow(slf)?;
        this.callables.push(callback.clone().unbind());
        this.inner.observe(Arc::new(PyObserver::new(callback.clone().unbind())));
        this.detachable = false;
        Ok(())
    }

    /// The runtime state as layout-v4 JSON text, formatted exactly like the Python library's
    /// `get_snapshot()` (`json.dumps(..., indent=2)`).
    fn snapshot(slf: &Bound<'_, Self>) -> PyResult<String> {
        let py = slf.py();
        let snap = Self::read(slf)?.inner.snapshot().map_err(|e| errors::to_py_err(py, &e))?;
        let value =
            serde_json::to_value(&snap).map_err(|e| errors::snapshot_error(py, e.to_string()))?;
        Ok(pyjson::dumps_indent(&value))
    }

    /// Alias of `snapshot()` (the Python library's name).
    fn get_snapshot(slf: &Bound<'_, Self>) -> PyResult<String> {
        Self::snapshot(slf)
    }

    /// `True` when the state with this id is active. Accepts full ids (`"m.a.b"`), custom
    /// `#id`s (without `#`) and paths relative to the root (`"a.b"`).
    fn matches(slf: &Bound<'_, Self>, state_id: &Bound<'_, PyString>) -> PyResult<bool> {
        let state_id = state_id.to_cow()?;
        let this = Self::read(slf)?;
        let machine = this.inner.machine();
        if machine.state_by_id(&state_id).is_some() {
            return Ok(this.inner.matches(&state_id));
        }
        Ok(this.inner.matches(&format!("{}.{}", machine.id(), &*state_id)))
    }

    /// Events emitted with the `emit` built-in since the last call, as dicts.
    fn take_emitted<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyList>> {
        let emitted = Self::borrow(slf)?.inner.take_emitted();
        let list = PyList::empty(slf.py());
        for event in &emitted {
            list.append(convert::to_py(slf.py(), event)?)?;
        }
        Ok(list)
    }

    /// `"uninitialized"` before `start()`, then `"running"`, `"done"`, `"error"` or
    /// `"stopped"`.
    #[getter]
    fn status(slf: &Bound<'_, Self>) -> PyResult<&'static str> {
        Ok(Self::read(slf)?.inner.state().map_or("uninitialized", |s| s.status().as_str()))
    }

    /// Active leaf state ids, sorted.
    #[getter]
    fn state_ids(slf: &Bound<'_, Self>) -> PyResult<Vec<String>> {
        Ok(Self::read(slf)?.inner.state_ids())
    }

    /// Active leaf state ids as a `set` (the Python library's `current_state_ids`).
    #[getter]
    fn current_state_ids<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PySet>> {
        PySet::new(slf.py(), Self::read(slf)?.inner.state_ids())
    }

    /// XState-style state value (`"idle"`, `{"a": "b"}`); `None` before `start()`.
    #[getter]
    fn value<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let this = Self::read(slf)?;
        let value = this.inner.state().map_or(Value::Null, |s| s.value(this.inner.machine()));
        convert::to_py(slf.py(), &value)
    }

    /// A copy of the context (mutating it does not change the machine); `None` before
    /// `start()`.
    #[getter]
    fn context<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let this = Self::read(slf)?;
        convert::to_py(slf.py(), this.inner.context().unwrap_or(&Value::Null))
    }

    /// Machine output once `"done"`, else `None`.
    #[getter]
    fn output<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let this = Self::read(slf)?;
        convert::to_py(slf.py(), this.inner.state().map_or(&Value::Null, ActiveState::output))
    }

    /// Error message once `"error"`, else `None`.
    #[getter]
    fn error(slf: &Bound<'_, Self>) -> PyResult<Option<String>> {
        Ok(Self::read(slf)?.inner.state().and_then(|s| s.error().map(str::to_owned)))
    }

    /// The machine this interpreter runs.
    #[getter]
    fn machine(slf: &Bound<'_, Self>) -> PyResult<PyMachine> {
        Ok(PyMachine::wrap(Self::read(slf)?.inner.machine().clone()))
    }

    /// Machine id.
    #[getter]
    fn id(slf: &Bound<'_, Self>) -> PyResult<String> {
        Ok(Self::read(slf)?.inner.machine().id().to_owned())
    }

    /// Pending (uncancelled) timers.
    #[getter]
    fn pending_timers(slf: &Bound<'_, Self>) -> PyResult<usize> {
        Ok(Self::read(slf)?.inner.pending_timers())
    }

    fn __repr__(slf: &Bound<'_, Self>) -> PyResult<String> {
        let this = Self::read(slf)?;
        let status = this.inner.state().map_or("uninitialized", |s| s.status().as_str());
        Ok(format!(
            "SyncInterpreter(id={:?}, status={status:?}, state_ids={:?})",
            this.inner.machine().id(),
            this.inner.state_ids()
        ))
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        for f in &self.callables {
            visit.call(f)?;
        }
        if let Some(clock) = &self.clock {
            visit.call(clock)?;
        }
        Ok(())
    }
}

impl PySyncInterpreter {
    fn borrow<'py>(slf: &Bound<'py, Self>) -> PyResult<PyRefMut<'py, Self>> {
        slf.try_borrow_mut().map_err(|_| errors::lifecycle_error(slf.py(), BUSY))
    }

    fn read<'py>(slf: &Bound<'py, Self>) -> PyResult<PyRef<'py, Self>> {
        slf.try_borrow().map_err(|_| errors::lifecycle_error(slf.py(), BUSY))
    }

    /// Register with the `SimulatedClock` (once) so `clock.increment()` fires our timers.
    fn attach_clock(slf: &Bound<'_, Self>) -> PyResult<()> {
        let clock = {
            let mut this = Self::borrow(slf)?;
            if this.clock_attached {
                return Ok(());
            }
            this.clock_attached = true;
            this.clock.as_ref().map(|c| c.clone_ref(slf.py()))
        };
        match clock {
            Some(clock) => clock.get().attach(slf.as_any()),
            None => Ok(()),
        }
    }

    /// Fire due timers on behalf of `SimulatedClock.increment()`; an interpreter that is busy
    /// (the increment came from one of its own callbacks) picks them up on its next tick.
    pub(crate) fn tick_from_clock(slf: &Bound<'_, Self>) -> PyResult<()> {
        if slf.try_borrow_mut().is_err() {
            return Ok(());
        }
        Self::run(slf, SyncInterpreter::tick).map(drop)
    }

    /// Run one engine operation, detached from Python when no callback can run, and turn the
    /// outcome into a receipt or an exception (chaining the callback's exception).
    fn run<F>(slf: &Bound<'_, Self>, op: F) -> PyResult<PyReceipt>
    where
        F: FnOnce(&mut SyncInterpreter<Value>) -> CoreResult<Receipt> + Send,
    {
        let py = slf.py();
        let mut this = Self::borrow(slf)?;
        this.slot.clear();
        let detachable = this.detachable;
        let inner = &mut this.inner;
        let result = if detachable { py.detach(move || op(inner)) } else { op(inner) };
        let parked = this.slot.take();
        drop(this);
        finish(py, result, parked)
    }
}

fn finish(
    py: Python<'_>,
    result: CoreResult<Receipt>,
    parked: Option<PyErr>,
) -> PyResult<PyReceipt> {
    // `KeyboardInterrupt` / `SystemExit` raised in a callback always propagate unchanged.
    if let Some(err) = parked.as_ref()
        && !err.is_instance_of::<PyException>(py)
    {
        return Err(err.clone_ref(py));
    }
    match result {
        Ok(receipt) => Ok(PyReceipt::new(receipt, parked.map(|e| e.into_value(py).into_any()))),
        Err(err) => Err(errors::with_cause(py, errors::to_py_err(py, &err), parked)),
    }
}

fn restart_policy(value: Option<&Bound<'_, PyAny>>) -> PyResult<RestartTimers> {
    let Some(value) = value.filter(|v| !v.is_none()) else { return Ok(RestartTimers::Resume) };
    if let Ok(flag) = value.cast::<PyBool>() {
        return Ok(if flag.is_true() { RestartTimers::Restart } else { RestartTimers::Parked });
    }
    let name: PyBackedStr = value.extract()?;
    match &*name {
        "resume" => Ok(RestartTimers::Resume),
        "parked" | "static" => Ok(RestartTimers::Parked),
        "fire_due" => Ok(RestartTimers::FireDue),
        "restart" => Ok(RestartTimers::Restart),
        other => Err(PyValueError::new_err(format!(
            "restart_timers must be 'resume', 'parked', 'fire_due', 'restart' or a bool, not {other:?}"
        ))),
    }
}
