//! Events and the engine-generated event names.
//!
//! An [`Event`] carries its XState `type` plus a JSON payload. Engine-generated events
//! (`xstate.done.state.*`, `xstate.done.actor.*`, `xstate.error.actor.*`, `xstate.after.*`,
//! `xstate.init`) are distinguished by [`EventOrigin`], not by their name, so a user cannot
//! forge a completion by sending an event with the right spelling.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an event came from. Only the engine can mint non-[`External`](EventOrigin::External)
/// events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    /// Sent by the application through `send`.
    External,
    /// Raised by a `raise` action or a self `sendTo` (internal queue).
    Internal,
    /// `xstate.done.state.<id>` after a child reached a final state.
    DoneState,
    /// `xstate.done.actor.<id>` after an invoked service completed.
    DoneActor,
    /// `xstate.error.actor.<id>` after an invoked service failed.
    ErrorActor,
    /// `xstate.after.<delay>.<state>` fired by a delayed transition timer.
    After,
    /// The implicit event that enters the initial configuration.
    Init,
}

/// An event delivered to a machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The XState event type, e.g. `"TOGGLE"` or `"xstate.done.actor.fetchUser"`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Payload fields. For user events this is the event object minus `type`; for
    /// completion events it is `{"output": ...}` or `{"error": ...}`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// Provenance; see [`EventOrigin`].
    #[serde(default = "EventOrigin::external")]
    pub origin: EventOrigin,
}

impl EventOrigin {
    const fn external() -> Self {
        Self::External
    }

    /// `true` for every origin except [`EventOrigin::External`].
    #[must_use]
    pub const fn is_engine_generated(self) -> bool {
        !matches!(self, Self::External)
    }
}

impl Event {
    /// A user event with no payload.
    pub fn new(ty: impl Into<String>) -> Self {
        Self { ty: ty.into(), payload: Value::Null, origin: EventOrigin::External }
    }

    /// A user event with a payload. Pass an object for XState-style flat payload fields.
    pub fn with_payload(ty: impl Into<String>, payload: Value) -> Self {
        Self { ty: ty.into(), payload, origin: EventOrigin::External }
    }

    /// Build a user event from an XState event object (`{"type": "ADD", "qty": 2}`), or from
    /// a bare string.
    ///
    /// # Errors
    /// Returns [`Error::InvalidEvent`](crate::Error::InvalidEvent) when the value is neither
    /// a string nor an object with a string `type`.
    pub fn from_json(value: Value) -> crate::Result<Self> {
        match value {
            Value::String(ty) => Ok(Self::new(ty)),
            Value::Object(mut map) => {
                let ty = match map.remove("type") {
                    Some(Value::String(ty)) => ty,
                    Some(other) => {
                        return Err(crate::Error::InvalidEvent(format!(
                            "event `type` must be a string, got {other}"
                        )));
                    }
                    None => {
                        return Err(crate::Error::InvalidEvent(
                            "event object has no `type`".into(),
                        ));
                    }
                };
                let payload = if map.is_empty() { Value::Null } else { Value::Object(map) };
                Ok(Self::new(ty).payload(payload))
            }
            other => Err(crate::Error::InvalidEvent(format!(
                "event must be a string or an object, got {other}"
            ))),
        }
    }

    /// Replace the payload (builder style).
    #[must_use]
    pub fn payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    pub(crate) fn engine(ty: String, payload: Value, origin: EventOrigin) -> Self {
        Self { ty, payload, origin }
    }

    /// The implicit start event.
    #[must_use]
    pub fn init() -> Self {
        Self::engine(names::INIT.to_owned(), Value::Null, EventOrigin::Init)
    }

    /// `xstate.done.state.<state id>` carrying the final state's output.
    #[must_use]
    pub fn done_state(state_id: &str, output: Value) -> Self {
        Self::engine(names::done_state(state_id), wrap("output", output), EventOrigin::DoneState)
    }

    /// `xstate.done.actor.<invoke id>` carrying the service output.
    #[must_use]
    pub fn done_actor(invoke_id: &str, output: Value) -> Self {
        Self::engine(names::done_actor(invoke_id), wrap("output", output), EventOrigin::DoneActor)
    }

    /// `xstate.error.actor.<invoke id>` carrying the error.
    #[must_use]
    pub fn error_actor(invoke_id: &str, error: Value) -> Self {
        Self::engine(names::error_actor(invoke_id), wrap("error", error), EventOrigin::ErrorActor)
    }

    /// `xstate.after.<delay>.<state id>`.
    #[must_use]
    pub fn after(delay_key: &str, state_id: &str) -> Self {
        Self::engine(names::after(delay_key, state_id), Value::Null, EventOrigin::After)
    }

    /// `event.output` for done events, `null` otherwise.
    #[must_use]
    pub fn output(&self) -> &Value {
        self.payload.get("output").unwrap_or(&Value::Null)
    }
}

fn wrap(key: &str, value: Value) -> Value {
    let mut map = serde_json::Map::with_capacity(1);
    map.insert(key.to_owned(), value);
    Value::Object(map)
}

/// Engine-generated event names, in their XState v5 spelling, plus the v4 aliases the parser
/// accepts on input (ADR-0001).
pub mod names {
    /// The implicit start event.
    pub const INIT: &str = "xstate.init";
    /// Catch-all event descriptor.
    pub const WILDCARD: &str = "*";

    /// `xstate.done.state.<id>`.
    #[must_use]
    pub fn done_state(state_id: &str) -> String {
        format!("xstate.done.state.{state_id}")
    }

    /// `xstate.done.actor.<id>`.
    #[must_use]
    pub fn done_actor(invoke_id: &str) -> String {
        format!("xstate.done.actor.{invoke_id}")
    }

    /// `xstate.error.actor.<id>`.
    #[must_use]
    pub fn error_actor(invoke_id: &str) -> String {
        format!("xstate.error.actor.{invoke_id}")
    }

    /// `xstate.after.<delay>.<state id>`.
    #[must_use]
    pub fn after(delay_key: &str, state_id: &str) -> String {
        format!("xstate.after.{delay_key}.{state_id}")
    }

    /// Rewrite an XState v4 / Python-dialect event name to its v5 spelling. Unknown names are
    /// returned unchanged.
    #[must_use]
    pub fn normalize(descriptor: &str) -> std::borrow::Cow<'_, str> {
        const ALIASES: [(&str, &str); 4] = [
            ("done.state.", "xstate.done.state."),
            ("done.invoke.", "xstate.done.actor."),
            ("error.platform.", "xstate.error.actor."),
            ("after.", "xstate.after."),
        ];
        for (old, new) in ALIASES {
            if let Some(rest) = descriptor.strip_prefix(old) {
                return std::borrow::Cow::Owned(format!("{new}{rest}"));
            }
        }
        std::borrow::Cow::Borrowed(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_json_splits_type_and_payload() {
        let ev = Event::from_json(serde_json::json!({"type": "ADD", "qty": 2})).unwrap();
        assert_eq!(ev.ty, "ADD");
        assert_eq!(ev.payload["qty"], 2);
        assert_eq!(ev.origin, EventOrigin::External);
        let bare = Event::from_json(Value::String("GO".into())).unwrap();
        assert_eq!(bare.payload, Value::Null);
    }

    #[test]
    fn v4_names_normalise() {
        assert_eq!(names::normalize("done.invoke.x"), "xstate.done.actor.x");
        assert_eq!(names::normalize("after.500.m.a"), "xstate.after.500.m.a");
        assert_eq!(names::normalize("TOGGLE"), "TOGGLE");
    }
}
