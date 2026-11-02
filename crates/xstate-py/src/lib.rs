//! Python bindings for the `xstate-core` engine, published as the `xstate_rs` extension module.
//!
//! The Python surface mirrors the synchronous API of the pure-Python `xstate-statemachine`
//! library (`create_machine`, `MachineLogic`, `SyncInterpreter`, receipts, snapshots), but every
//! step runs in the Rust engine. Snapshots are written in the Python library's layout v4 and
//! formatting, and `structure_hash` is identical, so the two engines share stores.
//!
//! ```python
//! import xstate_rs
//!
//! machine = xstate_rs.Machine.from_dict({
//!     "id": "toggle", "initial": "off",
//!     "states": {"off": {"on": {"FLIP": "on"}}, "on": {"on": {"FLIP": "off"}}},
//! })
//! interp = xstate_rs.SyncInterpreter(machine)
//! interp.start()
//! assert interp.send("FLIP").state_ids == {"toggle.on"}
//! ```
//!
//! Module layout: `convert` turns JSON values into Python objects and back, `logic` wraps
//! Python callables as engine implementations, `interpreter` drives the engine, `pyjson`
//! reproduces Python's `json.dumps(indent=2)` for snapshots, and `errors` maps engine errors
//! onto the exception hierarchy.

#![forbid(unsafe_code)]

mod clock;
mod convert;
mod errors;
mod event;
mod interpreter;
mod logic;
mod machine;
mod observer;
mod pyjson;
mod receipt;

use pyo3::prelude::*;

/// `structure_hash(machine)`: the 16-hex-character structure hash, identical to the Python
/// library's `persistence.structure_hash`.
#[pyfunction]
fn structure_hash(machine: &Bound<'_, machine::PyMachine>) -> String {
    machine.get().inner.structure_hash().to_owned()
}

/// The `xstate_rs` extension module.
#[pymodule]
fn xstate_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<machine::PyMachine>()?;
    m.add_class::<logic::PyMachineLogic>()?;
    m.add_class::<interpreter::PySyncInterpreter>()?;
    m.add_class::<receipt::PyReceipt>()?;
    m.add_class::<clock::PySimulatedClock>()?;
    m.add_function(wrap_pyfunction!(structure_hash, m)?)?;
    errors::register(m)?;
    m.add("SNAPSHOT_VERSION", xstate_core::SNAPSHOT_VERSION)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
