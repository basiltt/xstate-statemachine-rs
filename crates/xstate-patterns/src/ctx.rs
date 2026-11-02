//! Small helpers for editing a JSON (`DynContext`) context from action implementations.

use serde_json::{Map, Value};

/// Set `key` on the context object. A non-object context (for example `null`) is replaced by an
/// object holding just this key, so an action never has to fail on a malformed context.
pub(crate) fn set_key(context: &mut Value, key: &str, value: Value) {
    if let Some(map) = context.as_object_mut() {
        map.insert(key.to_owned(), value);
        return;
    }
    let mut map = Map::new();
    map.insert(key.to_owned(), value);
    *context = Value::Object(map);
}

/// Remove `key` from the context object (no-op for a non-object context).
pub(crate) fn remove_key(context: &mut Value, key: &str) {
    if let Some(map) = context.as_object_mut() {
        map.shift_remove(key);
    }
}

/// A non-negative integer counter stored under `key` (`0` when absent or not an integer).
pub(crate) fn counter(context: &Value, key: &str) -> u64 {
    context.get(key).and_then(Value::as_u64).unwrap_or(0)
}
