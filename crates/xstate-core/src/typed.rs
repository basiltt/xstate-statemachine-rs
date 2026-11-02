//! Runtime side of the `xstate-macros` derives: the [`TypedEvent`] and [`TypedState`] traits,
//! the chart checks they share, and the payload helpers the generated code calls.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::machine::{Trigger, partial_matches};
use crate::{Context, Error, Event, Machine, Result, SyncInterpreter};

/// An enum whose variants are a machine's events; implemented by `#[derive(TypedEvent)]`.
///
/// Converting into an [`Event`] serialises the payload; a payload that fails to serialise is
/// logged at `error` level and sent as `null`.
pub trait TypedEvent: Sized + Into<Event> + TryFrom<Event, Error = Error> {
    /// The XState `type` of this event.
    fn event_type(&self) -> &'static str;
    /// Every event type the enum declares, sorted.
    fn declared_events() -> &'static [&'static str];
    /// Compare the enum with the events `machine` handles.
    ///
    /// # Errors
    /// Lists the events the enum declares that no transition handles (exactly or through a
    /// wildcard) and the exact events the chart handles that the enum does not declare.
    fn check(machine: &Machine) -> Result<(), String> {
        let (declared, handled) = (Self::declared_events(), machine.declared_events());
        let wildcards: Vec<&str> = (machine.transitions().iter())
            .filter_map(|t| match &t.trigger {
                Trigger::Event(d) if d == "*" || d.ends_with(".*") => Some(d.as_str()),
                _ => None,
            })
            .collect();
        let unhandled = (declared.iter().copied())
            .filter(|e| !handled.iter().any(|h| h == e))
            .filter(|e| !wildcards.iter().any(|w| partial_matches(w, e)));
        let undeclared = (handled.iter().map(String::as_str)).filter(|h| !declared.contains(h));
        let (unhandled, undeclared) = (unhandled.collect(), undeclared.collect());
        let problems =
            [("unhandled by the chart", unhandled), ("missing from the enum", undeclared)];
        report(machine.id(), "events", &problems)
    }
}

/// An enum whose unit variants name a machine's leaf states; implemented by
/// `#[derive(TypedState)]`.
pub trait TypedState: Sized + 'static {
    /// Full state id (`door.opened`) this variant stands for.
    fn id(&self) -> &'static str;
    /// The variant whose id is `id`.
    fn from_id(id: &str) -> Option<Self>;
    /// Every variant, in declaration order.
    fn all() -> &'static [Self];
    /// `true` when this state is active in `interp`.
    fn is_active<C: Context>(&self, interp: &SyncInterpreter<C>) -> bool {
        interp.matches(self.id())
    }
    /// Compare the enum with the leaf states of `machine`.
    ///
    /// # Errors
    /// Lists ids that are not states, ids of non-leaf states, and leaves the enum lacks.
    fn check(machine: &Machine) -> Result<(), String> {
        let (mut unknown, mut non_leaf, mut covered) = (Vec::new(), Vec::new(), Vec::new());
        for id in Self::all().iter().map(Self::id) {
            match machine.state_by_id(id).map(|sid| machine.node(sid)) {
                None => unknown.push(id),
                Some(node) if !node.is_leaf() => non_leaf.push(id),
                Some(node) => covered.push(&*node.id),
            }
        }
        let leaves = machine.states().iter().filter(|n| n.is_leaf()).map(|n| &*n.id);
        let missing = leaves.filter(|l| !covered.contains(l)).collect();
        let problems = [
            ("unknown to the chart", unknown),
            ("that are not leaves", non_leaf),
            ("missing from the enum", missing),
        ];
        report(machine.id(), "states", &problems)
    }
}

fn report(machine: &str, noun: &str, problems: &[(&str, Vec<&str>)]) -> Result<(), String> {
    let parts: Vec<String> = (problems.iter().filter(|(_, list)| !list.is_empty()))
        .map(|(what, list)| format!("{noun} {what}: {}", list.join(", ")))
        .collect();
    if parts.is_empty() {
        Ok(())
    } else {
        Err(format!("machine `{machine}`: {}", parts.join("; ")))
    }
}

/// Serialise one payload value; failures are logged and become `null` (generated code only).
#[doc(hidden)]
pub fn encode<T: Serialize + ?Sized>(event_type: &str, value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| {
        tracing::error!(target: "xstate", event = event_type, error = %err, "payload dropped");
        Value::Null
    })
}

/// Build an object payload from named fields (generated code only).
#[doc(hidden)]
pub fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// Deserialise `payload`, or its field `field` (absent reads as `null`) (generated code only).
///
/// # Errors
/// [`Error::InvalidEvent`] naming the event (and field) when the payload has the wrong shape.
#[doc(hidden)]
pub fn decode<T: DeserializeOwned>(ty: &str, field: Option<&str>, payload: &Value) -> Result<T> {
    let value = field.map_or(Some(payload), |f| payload.get(f)).unwrap_or(&Value::Null);
    let at = field.map(|f| format!(" field `{f}`")).unwrap_or_default();
    T::deserialize(value).map_err(|e| Error::InvalidEvent(format!("`{ty}`{at}: {e}")))
}
