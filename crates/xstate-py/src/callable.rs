//! A Python callable shared between the engine closures that call it and the interpreter that
//! owns it, so the cyclic garbage collector can see (and break) every reference to it.
//!
//! A `Py<T>` cloned into an engine closure is a strong reference the interpreter's
//! `__traverse__` cannot report, so a cycle through a lambda or an observer (`interp.observe(
//! lambda ...: interp.state_ids)`) always looked externally referenced and was never freed.
//! Here each callable has exactly ONE owner, the slot; the closures hold the slot by `Arc`, the
//! interpreter reports the slot's reference from `__traverse__` and empties it in `__clear__`.

use std::sync::{Arc, Mutex, PoisonError};

use pyo3::prelude::*;
use pyo3::{PyTraverseError, PyVisit};
use xstate_core::Error;

/// The single owning reference to one Python callable; emptied by the garbage collector.
pub(crate) struct CallableSlot {
    callable: Mutex<Option<Py<PyAny>>>,
}

impl CallableSlot {
    pub(crate) fn new(callable: Py<PyAny>) -> Arc<Self> {
        Arc::new(Self { callable: Mutex::new(Some(callable)) })
    }

    /// A temporary strong reference for one call, or `None` once the slot was cleared.
    pub(crate) fn get(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.guard().as_ref().map(|f| f.clone_ref(py))
    }

    /// Like [`get`](Self::get), as the engine error a cleared callback raises.
    pub(crate) fn get_or_err(&self, py: Python<'_>) -> Result<Py<PyAny>, Error> {
        self.get(py).ok_or_else(|| {
            Error::Logic("the Python callback was released by the garbage collector".to_owned())
        })
    }

    /// Report the owned reference to the garbage collector.
    pub(crate) fn visit(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        match self.guard().as_ref() {
            Some(callable) => visit.call(callable),
            None => Ok(()),
        }
    }

    /// Drop the owned reference (`__clear__`); later calls fail with a logic error.
    pub(crate) fn clear(&self) {
        // Release the lock before the decref: dropping can run arbitrary `__del__` code.
        let released = self.guard().take();
        drop(released);
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Option<Py<PyAny>>> {
        self.callable.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
