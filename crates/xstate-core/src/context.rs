//! The extended-state ("context") contract.
//!
//! The engine is generic over the context type (ADR-0002). [`DynContext`] (a
//! [`serde_json::Value`]) is the default used by JSON-driven machines; any serialisable,
//! clonable type can be used instead.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Dynamic context: a JSON value, normally an object.
pub type DynContext = Value;

/// Requirements on a context type.
///
/// `Clone` backs the `rollback` action-error policy and snapshots; `Serialize`/`Deserialize`
/// back persistence and the JSON-form `assign` action.
pub trait Context: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// Apply a JSON-form `assign` patch: every key of `patch` replaces the same-named field.
    ///
    /// The default implementation round-trips through JSON; [`DynContext`] patches in place.
    ///
    /// # Errors
    /// Fails when the context cannot be represented as a JSON object or the patched object
    /// does not deserialise back into `Self`.
    fn assign_json(&mut self, patch: &Map<String, Value>) -> crate::Result<()> {
        let mut value = serde_json::to_value(&*self)?;
        assign_into_value(&mut value, patch);
        *self = serde_json::from_value(value)?;
        Ok(())
    }

    /// Build the initial context from the machine's `context` value (or `{}`), given the
    /// actor `input`.
    ///
    /// # Errors
    /// Fails when the JSON does not deserialise into `Self`.
    fn from_initial(value: &Value, _input: &Value) -> crate::Result<Self> {
        Ok(serde_json::from_value(value.clone())?)
    }
}

fn assign_into_value(value: &mut Value, patch: &Map<String, Value>) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    if let Value::Object(map) = value {
        for (k, v) in patch {
            map.insert(k.clone(), v.clone());
        }
    }
}

impl Context for Value {
    fn assign_json(&mut self, patch: &Map<String, Value>) -> crate::Result<()> {
        assign_into_value(self, patch);
        Ok(())
    }

    fn from_initial(value: &Value, _input: &Value) -> crate::Result<Self> {
        Ok(if value.is_null() { Value::Object(Map::new()) } else { value.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dyn_context_assigns_in_place() {
        let mut ctx = serde_json::json!({"a": 1});
        let patch = serde_json::json!({"b": 2, "a": 3});
        ctx.assign_json(patch.as_object().unwrap()).unwrap();
        assert_eq!(ctx, serde_json::json!({"a": 3, "b": 2}));
    }

    #[test]
    fn typed_context_assigns_via_json() {
        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Cart {
            items: u32,
            paid: bool,
        }
        impl Context for Cart {}
        let mut cart = Cart { items: 0, paid: false };
        let patch = serde_json::json!({"items": 4});
        cart.assign_json(patch.as_object().unwrap()).unwrap();
        assert_eq!(cart, Cart { items: 4, paid: false });
    }
}
