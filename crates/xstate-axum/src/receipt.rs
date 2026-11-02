//! Response bodies: the state body, the receipt body and `Receipt → status` (Python `_http.py`
//! `state_body`, `receipt_body`, `receipt_to_status`, `available_events`).
//!
//! X0.1: context is included **only** through an explicit [`ContextSerializer`], and the
//! library's `_xsm_` bookkeeping keys never reach a body.

use std::sync::Arc;

use serde_json::{Map, Value};
use xstate_core::config::UnhandledPolicy;
use xstate_core::machine::Trigger;
use xstate_core::{ActiveState, Event, Machine, MachineLogic, Receipt, Status};
use xstate_persistence::receipts::ReceiptErrorKind;
use xstate_persistence::{ReceiptFlags, ReceiptJson};

/// Internal event families never offered to a client as "available" (nor routed).
pub const INTERNAL_PREFIXES: [&str; 4] = ["done.", "error.", "after.", "xstate."];

/// Prefixes of library-private context keys dropped from API bodies.
const PRIVATE_CONTEXT_PREFIXES: [&str; 2] = ["_xsm_", "__xsm_"];

/// `(public context) -> JSON` for response bodies (Python `context_serializer`).
pub type ContextSerializer = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// `true` for an engine-internal event type (`done.*`, `error.*`, `after.*`, `xstate.*`).
#[must_use]
pub fn is_internal_event(event_type: &str) -> bool {
    INTERNAL_PREFIXES.iter().any(|p| event_type.starts_with(p))
}

/// Declared, client-sendable event names, sorted (Python `declared_events`).
#[must_use]
pub fn declared_events(machine: &Machine) -> Vec<String> {
    let mut events: Vec<String> = machine
        .declared_events()
        .iter()
        .filter(|e| !is_internal_event(e) && !e.contains('*'))
        .cloned()
        .collect();
    events.sort();
    events.dedup();
    events
}

/// A shallow copy of a context object without the library-private keys (Python
/// `public_context`). Non-object contexts are returned unchanged.
#[must_use]
pub fn public_context(context: &Value) -> Value {
    match context {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !PRIVATE_CONTEXT_PREFIXES.iter().any(|p| k.starts_with(p)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A read-only view of one instance: what the body builders need.
#[derive(Clone, Copy)]
pub struct StateView<'a> {
    machine: &'a Machine,
    logic: &'a MachineLogic<Value>,
    state: &'a ActiveState<Value>,
}

impl std::fmt::Debug for StateView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateView").field("machine", &self.machine.id()).finish_non_exhaustive()
    }
}

impl<'a> StateView<'a> {
    /// View `state` of `machine` (guards come from `logic`).
    #[must_use]
    pub fn new(
        machine: &'a Machine,
        logic: &'a MachineLogic<Value>,
        state: &'a ActiveState<Value>,
    ) -> Self {
        Self { machine, logic, state }
    }

    /// XState state value (`"idle"`, `{"a": "b"}`).
    #[must_use]
    pub fn value(&self) -> Value {
        self.state.value(self.machine)
    }

    /// Active leaf ids, sorted.
    #[must_use]
    pub fn state_ids(&self) -> Vec<String> {
        self.state.leaf_ids(self.machine)
    }

    /// Whether `event_type` would take a transition right now (Python `can()`): guards are
    /// evaluated through the pure transition API, nothing runs. A transition is "taken" when
    /// the configuration or context changes or an action would run; a targetless transition
    /// with no action is not observable this way and reports `false`.
    #[must_use]
    pub fn can(&self, event_type: &str) -> bool {
        if self.state.status() != Status::Running {
            return false;
        }
        match xstate_core::pure::transition(
            self.machine,
            self.logic,
            self.state,
            Event::new(event_type),
        ) {
            Ok((next, planned)) => {
                !planned.is_empty()
                    || !next.configuration().eq(self.state.configuration())
                    || next.context() != self.state.context()
            }
            Err(error) => {
                tracing::debug!(target: "xstate::axum", %error, event_type, "can() failed; reporting false");
                false
            }
        }
    }

    /// User-facing events that would cause a transition right now.
    #[must_use]
    pub fn available_events(&self) -> Vec<String> {
        declared_events(self.machine).into_iter().filter(|e| self.can(e)).collect()
    }

    /// The instance context.
    #[must_use]
    pub fn context(&self) -> &Value {
        self.state.context()
    }
}

/// `{state, state_ids, available_events[, context]}` (Python `state_body`).
#[must_use]
pub fn state_body(view: &StateView<'_>, serializer: Option<&ContextSerializer>) -> Value {
    let mut body = Map::new();
    body.insert("state".into(), view.value());
    body.insert("state_ids".into(), Value::from(view.state_ids()));
    body.insert("available_events".into(), Value::from(view.available_events()));
    if let Some(serialize) = serializer {
        body.insert("context".into(), serialize(&public_context(view.context())));
    }
    Value::Object(body)
}

/// The receipt body of `POST` routes and WebSocket replies (Python `receipt_body`): the state
/// body with the receipt's `state_ids`, plus `changed`, `denied`, `deferred`, `duplicate` and
/// `error` (the class name only, X0.7).
#[must_use]
pub fn receipt_body(
    view: &StateView<'_>,
    receipt: &ReceiptJson,
    serializer: Option<&ContextSerializer>,
) -> Value {
    let mut body = state_body(view, serializer);
    if let Value::Object(map) = &mut body {
        let mut ids = receipt.state_ids.clone();
        ids.sort();
        map.insert("state_ids".into(), Value::from(ids));
        map.insert("changed".into(), Value::Bool(receipt.changed));
        map.insert("denied".into(), Value::Bool(receipt.denied));
        map.insert("deferred".into(), Value::Bool(receipt.deferred));
        map.insert("duplicate".into(), Value::Bool(receipt.duplicate));
        let error = receipt.error.as_ref().map(|e| e.kind.type_name().to_owned());
        map.insert("error".into(), Value::from(error));
    }
    body
}

/// Map a receipt to an HTTP status with the starlette precedence: an idempotency refusal
/// (mismatch 422, in flight 409) beats `duplicate` (200), which beats the core mapping of
/// [`xstate_persistence::receipt_to_status`] (error, deferred 202, denied 409, 200).
#[must_use]
pub fn receipt_to_status(receipt: &ReceiptJson) -> u16 {
    match receipt.error.as_ref().map(|e| &e.kind) {
        Some(ReceiptErrorKind::IdempotencyMismatch) => 422,
        Some(ReceiptErrorKind::IdempotencyInFlight) => 409,
        _ if receipt.duplicate => 200,
        _ => xstate_persistence::receipt_to_status(receipt),
    }
}

/// The receipt flags the engine receipt does not carry: `deferred` (unhandled under
/// `onUnhandled: "defer"`) and `denied` (unhandled although a transition for the event existed
/// in the configuration `before` the send, i.e. a guard refused it).
#[must_use]
pub fn receipt_flags(
    machine: &Machine,
    before: &ActiveState<Value>,
    event_type: &str,
    receipt: &Receipt,
) -> ReceiptFlags {
    if !receipt.unhandled || receipt.status != Status::Running {
        return ReceiptFlags::default();
    }
    if machine.policies().on_unhandled == UnhandledPolicy::Defer {
        return ReceiptFlags { deferred: true, ..ReceiptFlags::default() };
    }
    ReceiptFlags { denied: has_candidate(machine, before, event_type), ..ReceiptFlags::default() }
}

/// `true` when an active state declares a transition for `event_type`.
fn has_candidate(machine: &Machine, state: &ActiveState<Value>, event_type: &str) -> bool {
    machine.transitions().iter().any(|t| {
        state.is_active(t.source)
            && matches!(&t.trigger, Trigger::Event(desc) if descriptor_matches(desc, event_type))
    })
}

fn descriptor_matches(descriptor: &str, event_type: &str) -> bool {
    if descriptor == event_type || descriptor == "*" {
        return true;
    }
    descriptor.strip_suffix(".*").is_some_and(|prefix| {
        event_type == prefix
            || event_type.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xstate_persistence::receipts::ReceiptError;

    #[test]
    fn private_keys_are_dropped() {
        let ctx = serde_json::json!({"a": 1, "_xsm_errors": [], "__xsm_processed_ids__": []});
        assert_eq!(public_context(&ctx), serde_json::json!({"a": 1}));
    }

    #[test]
    fn status_precedence() {
        let dup = ReceiptJson { duplicate: true, denied: true, ..ReceiptJson::default() };
        assert_eq!(receipt_to_status(&dup), 200);
        let mismatch = ReceiptJson {
            duplicate: true,
            error: Some(ReceiptError::new(ReceiptErrorKind::IdempotencyMismatch, "m")),
            ..ReceiptJson::default()
        };
        assert_eq!(receipt_to_status(&mismatch), 422);
        assert_eq!(receipt_to_status(&ReceiptJson { denied: true, ..ReceiptJson::default() }), 409);
        assert_eq!(
            receipt_to_status(&ReceiptJson { deferred: true, ..ReceiptJson::default() }),
            202
        );
    }

    #[test]
    fn wildcard_descriptors() {
        assert!(descriptor_matches("a.*", "a.b"));
        assert!(descriptor_matches("a.*", "a"));
        assert!(!descriptor_matches("a.*", "ab"));
        assert!(descriptor_matches("*", "x"));
    }
}
