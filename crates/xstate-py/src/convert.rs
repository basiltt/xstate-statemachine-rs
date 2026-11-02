//! Direct JSON <-> Python object conversion (no intermediate JSON text).
//!
//! Python → JSON accepts `dict` (string or integer keys), `list`, `tuple`, `str`, `int`,
//! `float`, `bool` and `None`, including subclasses (`OrderedDict`, `IntEnum`, ...). Integers
//! must fit in 64 bits, floats must be finite, and nesting is capped at [`MAX_DEPTH`] so a
//! self-referencing container fails cleanly instead of overflowing the stack.

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Number, Value};
use xstate_core::Event;

/// Deepest container nesting accepted from Python.
pub(crate) const MAX_DEPTH: usize = 256;

/// Convert a JSON value into a fresh Python object.
pub(crate) fn to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => py.None().into_bound(py),
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Number(n) => number_to_py(py, n)?,
        Value::String(s) => PyString::new(py, s).into_any(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(to_py(py, item)?)?;
            }
            list.into_any()
        }
        Value::Object(map) => map_to_py(py, map)?.into_any(),
    })
}

/// Convert a JSON object into a fresh Python `dict`.
pub(crate) fn map_to_py<'py>(
    py: Python<'py>,
    map: &Map<String, Value>,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (key, value) in map {
        dict.set_item(PyString::new(py, key), to_py(py, value)?)?;
    }
    Ok(dict)
}

fn number_to_py<'py>(py: Python<'py>, n: &Number) -> PyResult<Bound<'py, PyAny>> {
    if let Some(i) = n.as_i64() {
        return Ok(i.into_pyobject(py)?.into_any());
    }
    if let Some(u) = n.as_u64() {
        return Ok(u.into_pyobject(py)?.into_any());
    }
    Ok(PyFloat::new(py, n.as_f64().unwrap_or(f64::NAN)).into_any())
}

/// Convert a Python object into a JSON value.
///
/// # Errors
/// `TypeError` for unsupported types or keys, `ValueError` for out-of-range numbers and for
/// nesting deeper than [`MAX_DEPTH`].
pub(crate) fn from_py(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    from_py_at(obj, 0)
}

fn from_py_at(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
    if depth > MAX_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nests deeper than {MAX_DEPTH} levels (is it self-referencing?)"
        )));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::String(s.to_cow()?.into_owned()));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if let Ok(i) = obj.cast::<PyInt>() {
        return int_to_json(i);
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return float_to_json(f.value());
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        return Ok(Value::Object(dict_to_map_at(d, depth)?));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(from_py_at(&item, depth + 1)?);
        }
        return Ok(Value::Array(out));
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        let mut out = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            out.push(from_py_at(&item, depth + 1)?);
        }
        return Ok(Value::Array(out));
    }
    Err(PyTypeError::new_err(format!(
        "object of type '{}' is not JSON-serialisable",
        type_name(obj)
    )))
}

/// Convert a Python `dict` into a JSON object.
///
/// # Errors
/// See [`from_py`].
pub(crate) fn dict_to_map(dict: &Bound<'_, PyDict>) -> PyResult<Map<String, Value>> {
    dict_to_map_at(dict, 0)
}

fn dict_to_map_at(dict: &Bound<'_, PyDict>, depth: usize) -> PyResult<Map<String, Value>> {
    let mut map = Map::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        map.insert(key_to_string(&key)?, from_py_at(&value, depth + 1)?);
    }
    Ok(map)
}

fn key_to_string(key: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(s) = key.cast::<PyString>() {
        return Ok(s.to_cow()?.into_owned());
    }
    if key.cast::<PyInt>().is_ok() && key.cast::<PyBool>().is_err() {
        return Ok(key.str()?.to_cow()?.into_owned());
    }
    Err(PyTypeError::new_err(format!("dict keys must be str or int, not '{}'", type_name(key))))
}

fn int_to_json(i: &Bound<'_, PyInt>) -> PyResult<Value> {
    if let Ok(v) = i.extract::<i64>() {
        return Ok(Value::from(v));
    }
    if let Ok(v) = i.extract::<u64>() {
        return Ok(Value::from(v));
    }
    Err(PyValueError::new_err("integer does not fit in 64 bits and cannot be stored as JSON"))
}

fn float_to_json(f: f64) -> PyResult<Value> {
    Number::from_f64(f).map(Value::Number).ok_or_else(|| {
        PyValueError::new_err(format!("{f} is not a finite float and cannot be stored as JSON"))
    })
}

/// The event as an XState-style Python dict: `{"type": ..., **payload}`. A payload that is
/// not an object is exposed under `"payload"`.
pub(crate) fn event_to_py<'py>(py: Python<'py>, event: &Event) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item(pyo3::intern!(py, "type"), PyString::new(py, &event.ty))?;
    match &event.payload {
        Value::Null => {}
        Value::Object(map) => {
            for (key, value) in map {
                if key != "type" {
                    dict.set_item(PyString::new(py, key), to_py(py, value)?)?;
                }
            }
        }
        other => dict.set_item(pyo3::intern!(py, "payload"), to_py(py, other)?)?,
    }
    Ok(dict)
}

/// `type(obj).__name__`, or `"object"` when it cannot be read.
pub(crate) fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .name()
        .and_then(|n| n.to_cow().map(std::borrow::Cow::into_owned))
        .unwrap_or_else(|_| "object".to_owned())
}
