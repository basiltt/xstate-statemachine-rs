//! The exception hierarchy and the mapping from [`xstate_core::Error`].
//!
//! Every exception raised by the engine derives from `XStateError` and carries a `kind`
//! attribute with the stable [`xstate_core::ErrorKind`] name (`"invalid_config"`,
//! `"transition_failed"`, ...). When the failure started in a Python callback, the original
//! exception is chained as `__cause__`.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use xstate_core::{Error, ErrorKind};

create_exception!(
    xstate_rs,
    XStateError,
    PyException,
    "Base class of every error raised by the xstate_rs engine."
);
create_exception!(
    xstate_rs,
    ConfigError,
    XStateError,
    "The machine configuration is malformed or violates a structural rule."
);
create_exception!(
    xstate_rs,
    StateNotFoundError,
    ConfigError,
    "A state id or transition target does not resolve."
);
create_exception!(
    xstate_rs,
    ImplementationMissingError,
    ConfigError,
    "A named action, guard, service or delay has no implementation."
);
create_exception!(
    xstate_rs,
    InvalidEventError,
    XStateError,
    "An event could not be normalised (wrong shape, missing or non-string type)."
);
create_exception!(
    xstate_rs,
    UnknownEventError,
    XStateError,
    "An undeclared event was sent to a strict machine."
);
create_exception!(
    xstate_rs,
    InvalidLifecycleError,
    XStateError,
    "The interpreter is not in a state that allows the operation."
);
create_exception!(
    xstate_rs,
    LogicError,
    XStateError,
    "User logic (an action, guard, service or delay) failed."
);
create_exception!(
    xstate_rs,
    TransitionFailedError,
    LogicError,
    "An action failed under actionErrorPolicy 'fail'; the machine moved to 'error'."
);
create_exception!(
    xstate_rs,
    RunawayChainError,
    LogicError,
    "The maxIterations budget for eventless or self-generated events was exhausted."
);
create_exception!(
    xstate_rs,
    SnapshotError,
    XStateError,
    "A snapshot could not be produced or restored (version, drift, corruption)."
);
create_exception!(
    xstate_rs,
    LimitExceededError,
    XStateError,
    "A payload size, depth or count limit was exceeded."
);

/// Add every exception class to the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("XStateError", py.get_type::<XStateError>())?;
    m.add("ConfigError", py.get_type::<ConfigError>())?;
    m.add("StateNotFoundError", py.get_type::<StateNotFoundError>())?;
    m.add("ImplementationMissingError", py.get_type::<ImplementationMissingError>())?;
    m.add("InvalidEventError", py.get_type::<InvalidEventError>())?;
    m.add("UnknownEventError", py.get_type::<UnknownEventError>())?;
    m.add("InvalidLifecycleError", py.get_type::<InvalidLifecycleError>())?;
    m.add("LogicError", py.get_type::<LogicError>())?;
    m.add("TransitionFailedError", py.get_type::<TransitionFailedError>())?;
    m.add("RunawayChainError", py.get_type::<RunawayChainError>())?;
    m.add("SnapshotError", py.get_type::<SnapshotError>())?;
    m.add("LimitExceededError", py.get_type::<LimitExceededError>())?;
    Ok(())
}

/// Convert an engine error into the matching Python exception, with its `kind` attribute set.
pub(crate) fn to_py_err(py: Python<'_>, err: &Error) -> PyErr {
    let message = err.to_string();
    let kind = err.kind();
    let exc = match kind {
        ErrorKind::InvalidConfig | ErrorKind::RootTarget => ConfigError::new_err(message),
        ErrorKind::StateNotFound => StateNotFoundError::new_err(message),
        ErrorKind::ImplementationMissing => ImplementationMissingError::new_err(message),
        ErrorKind::InvalidEvent => InvalidEventError::new_err(message),
        ErrorKind::UnknownEvent => UnknownEventError::new_err(message),
        ErrorKind::InvalidLifecycle => InvalidLifecycleError::new_err(message),
        ErrorKind::TransitionFailed => TransitionFailedError::new_err(message),
        ErrorKind::RunawayChain => RunawayChainError::new_err(message),
        ErrorKind::Snapshot => SnapshotError::new_err(message),
        ErrorKind::LimitExceeded => LimitExceededError::new_err(message),
        _ => LogicError::new_err(message),
    };
    with_kind(py, exc, &kind.to_string())
}

/// A configuration error (used where a JSON parse failure means a bad document, not a bad
/// event).
pub(crate) fn config_error(py: Python<'_>, err: &Error) -> PyErr {
    if matches!(err, Error::Json(_)) {
        return with_kind(py, ConfigError::new_err(err.to_string()), "invalid_config");
    }
    to_py_err(py, err)
}

/// A snapshot error raised by the bindings themselves.
pub(crate) fn snapshot_error(py: Python<'_>, message: String) -> PyErr {
    with_kind(py, SnapshotError::new_err(message), "snapshot")
}

/// An invalid-lifecycle error raised by the bindings themselves.
pub(crate) fn lifecycle_error(py: Python<'_>, message: &str) -> PyErr {
    with_kind(py, InvalidLifecycleError::new_err(message.to_owned()), "invalid_lifecycle")
}

/// An invalid-event error raised by the bindings themselves.
pub(crate) fn event_error(py: Python<'_>, message: String) -> PyErr {
    with_kind(py, InvalidEventError::new_err(message), "invalid_event")
}

fn with_kind(py: Python<'_>, exc: PyErr, kind: &str) -> PyErr {
    // Setting an attribute on a fresh exception instance cannot fail in practice; if it did,
    // the exception is still raised, just without `kind`.
    let _ = exc.value(py).setattr("kind", kind);
    exc
}

/// Chain `cause` (the Python exception raised by a callback) under `exc`.
pub(crate) fn with_cause(py: Python<'_>, exc: PyErr, cause: Option<PyErr>) -> PyErr {
    if cause.is_some() {
        exc.set_cause(py, cause);
    }
    exc
}
