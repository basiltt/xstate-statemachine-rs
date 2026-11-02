//! Normalising `send()` arguments into engine events.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};
use serde_json::{Map, Value};
use smol_str::SmolStr;
use xstate_core::Event;

use crate::convert;
use crate::errors;

/// How many distinct event-type objects [`TypeCache`] remembers.
const TYPE_CACHE_SIZE: usize = 8;

/// Recently sent event-type strings, matched by object identity. A string literal in calling
/// code is the same object on every call, so its UTF-8 copy is reused instead of re-encoded.
/// Holding a strong reference keeps each key alive, so an address can never be reused by a
/// different string while it is cached.
#[derive(Default)]
pub(crate) struct TypeCache {
    entries: Vec<(Py<PyString>, SmolStr)>,
    next: usize,
}

impl TypeCache {
    fn get(&mut self, s: &Bound<'_, PyString>) -> PyResult<SmolStr> {
        if let Some((_, ty)) = self.entries.iter().find(|(key, _)| key.as_ptr() == s.as_ptr()) {
            return Ok(ty.clone());
        }
        let ty = SmolStr::from(s.to_cow()?.as_ref());
        let entry = (s.clone().unbind(), ty.clone());
        if self.entries.len() < TYPE_CACHE_SIZE {
            self.entries.push(entry);
        } else {
            self.entries[self.next] = entry;
            self.next = (self.next + 1) % TYPE_CACHE_SIZE;
        }
        Ok(ty)
    }
}

/// Build an engine event from `send(event, **payload)`. `event` is a type string, an XState
/// event dict (`{"type": "ADD", "qty": 2}`) or an object with `type` and an optional dict
/// `payload` attribute (the Python library's `Event`); keyword arguments extend the payload.
pub(crate) fn build_event(
    event: &Bound<'_, PyAny>,
    extra: Option<&Bound<'_, PyDict>>,
    cache: &mut TypeCache,
) -> PyResult<Event> {
    let py = event.py();
    let (ty, mut payload) = if let Ok(s) = event.cast::<PyString>() {
        (cache.get(s)?, Map::new())
    } else if let Ok(dict) = event.cast::<PyDict>() {
        let mut map = convert::dict_to_map(dict)?;
        match map.remove("type") {
            Some(Value::String(ty)) => (SmolStr::from(ty), map),
            _ => return Err(errors::event_error(py, "an event dict needs a string 'type'".into())),
        }
    } else if event.hasattr(pyo3::intern!(py, "type"))? {
        object_event(event)?
    } else {
        return Err(errors::event_error(
            py,
            format!(
                "an event must be a str, a dict or an Event, not '{}'",
                convert::type_name(event)
            ),
        ));
    };
    if let Some(extra) = extra {
        for (key, value) in extra.iter() {
            let key = key.cast::<PyString>()?.to_cow()?;
            // The Python library's `send(..., wait=, priority=)` flags: never payload there,
            // and meaningless here (every send already runs to completion and returns a receipt).
            if key != "wait" && key != "priority" {
                payload.insert(key.into_owned(), convert::from_py(&value)?);
            }
        }
    }
    if ty.is_empty() {
        return Err(errors::event_error(py, "the event type must be a non-empty string".into()));
    }
    let payload = if payload.is_empty() { Value::Null } else { Value::Object(payload) };
    Ok(Event::with_payload(ty, payload))
}

/// An object with `type` and an optional dict `payload` (the Python library's `Event`).
fn object_event(event: &Bound<'_, PyAny>) -> PyResult<(SmolStr, Map<String, Value>)> {
    let py = event.py();
    let ty = event.getattr(pyo3::intern!(py, "type"))?;
    let ty = ty
        .cast::<PyString>()
        .map_err(|_| errors::event_error(py, "the event type must be a string".into()))?
        .to_cow()?;
    let payload = match event.getattr_opt(pyo3::intern!(py, "payload"))? {
        Some(p) if !p.is_none() => {
            let dict = p
                .cast::<PyDict>()
                .map_err(|_| errors::event_error(py, "the event payload must be a dict".into()))?;
            convert::dict_to_map(dict)?
        }
        _ => Map::new(),
    };
    Ok((SmolStr::from(ty.as_ref()), payload))
}
