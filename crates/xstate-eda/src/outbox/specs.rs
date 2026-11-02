//! What does the chart publish? (Python `publish_specs` and `_transition_spec`.)

use serde_json::{Map, Value};
use xstate_core::Machine;
use xstate_core::machine::Transition;

use super::PUBLISH_TAG;
use crate::chart::{ChartNode, config_has_tag, state_key, transition_rows, walk};
use crate::error::{EdaError, Result};

/// Where a publication is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishSource {
    /// A transition's `meta.publish`.
    Transition,
    /// A state tagged [`PUBLISH_TAG`].
    State,
}

impl PublishSource {
    /// `"transition"` / `"state"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transition => "transition",
            Self::State => "state",
        }
    }
}

/// One publication the chart declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishSpec {
    /// CloudEvents type published.
    pub ty: String,
    /// Transition or state.
    pub source: PublishSource,
    /// Id of the declaring state (the transition's source state).
    pub from: String,
    /// The event label of a transition (Python spelling: `after.500`, `done.invoke.x`,
    /// `error.platform.x`, `done.state.<id>`, `""` for `always`); `None` for a state.
    pub event: Option<String>,
    /// Context fields copied into `data` (`meta.publish.data`), if declared.
    pub fields: Option<Vec<String>>,
}

impl PublishSpec {
    /// The Python dict shape `{"type", "source", "from", "event", "fields"}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::from(self.ty.clone()));
        m.insert("source".into(), Value::from(self.source.as_str()));
        m.insert("from".into(), Value::from(self.from.clone()));
        m.insert("event".into(), self.event.clone().map_or(Value::Null, Value::from));
        m.insert(
            "fields".into(),
            self.fields.as_ref().map_or(Value::Null, |f| Value::from(f.clone())),
        );
        Value::Object(m)
    }
}

/// `xsm.<machine>.transition.<state key>`.
pub(crate) fn state_type(machine_id: &str, state_id: &str) -> String {
    format!("xsm.{machine_id}.transition.{}", state_key(state_id, machine_id))
}

/// A parsed `meta.publish`: `(type, fields)`; `None` when absent / `false`.
///
/// `target` is the id of the state a `true` spec is named after (the target, else the source).
pub(crate) fn parse_spec(
    meta: Option<&Value>,
    machine_id: &str,
    target: impl FnOnce() -> String,
) -> Result<Option<(String, Option<Vec<String>>)>> {
    let Some(spec) = meta.and_then(|m| m.get("publish")) else { return Ok(None) };
    match spec {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Bool(true) => Ok(Some((state_type(machine_id, &target()), None))),
        Value::String(s) => Ok(Some((s.clone(), None))),
        Value::Object(obj) if obj.get("type").is_some_and(Value::is_string) => {
            let ty = obj.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
            let fields = match obj.get("data") {
                None | Some(Value::Null) => None,
                Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                    Some(items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                }
                Some(_) => {
                    return Err(EdaError::InvalidSpec(
                        "meta.publish.data must be a list of context field names".into(),
                    ));
                }
            };
            Ok(Some((ty, fields)))
        }
        _ => Err(EdaError::InvalidSpec(
            "meta.publish must be true, an event type string or {\"type\": ..., \"data\": [fields]}"
                .into(),
        )),
    }
}

/// The spec of a compiled transition (what [`OutboxPlugin`](super::OutboxPlugin) publishes).
pub(crate) fn transition_spec(
    machine: &Machine,
    t: &Transition,
) -> Result<Option<(String, Option<Vec<String>>)>> {
    parse_spec(t.meta.as_ref(), machine.id(), || {
        let sid = t.targets.first().copied().unwrap_or(t.source);
        machine.node(sid).id.to_string()
    })
}

/// Every publication the chart declares, in document order, for AsyncAPI and docs (Python
/// `publish_specs`). Walks the chart document ([`Machine::to_json`]) so event labels keep
/// their Python spelling.
///
/// # Errors
/// [`EdaError::InvalidSpec`] for a malformed `meta.publish`.
pub fn publish_specs(machine: &Machine) -> Result<Vec<PublishSpec>> {
    let doc = machine.to_json();
    let mid = machine.id();
    let mut out = Vec::new();
    for node in walk(&doc, mid) {
        if config_has_tag(node.config, PUBLISH_TAG) {
            out.push(PublishSpec {
                ty: state_type(mid, &node.id),
                source: PublishSource::State,
                from: node.id.clone(),
                event: None,
                fields: None,
            });
        }
        for (event, transitions) in transition_rows(&node) {
            for t in &transitions {
                let meta = t.get("meta");
                let Some((ty, fields)) =
                    parse_spec(meta, mid, || resolve_target(machine, &node, t))?
                else {
                    continue;
                };
                out.push(PublishSpec {
                    ty,
                    source: PublishSource::Transition,
                    from: node.id.clone(),
                    event: Some(event.clone()),
                    fields,
                });
            }
        }
    }
    Ok(out)
}

/// The id of the state a JSON transition targets, resolved through the compiled machine (the
/// source when targetless or unresolvable).
fn resolve_target(machine: &Machine, node: &ChartNode<'_>, t: &Map<String, Value>) -> String {
    let raw = match t.get("target") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(items)) => items.first().and_then(Value::as_str),
        _ => None,
    };
    let Some(raw) = raw else { return node.id.clone() };
    let Some(source) = machine.state_by_id(&node.id) else { return node.id.clone() };
    machine
        .transitions()
        .iter()
        .find(|ct| ct.source == source && ct.raw_targets.first().is_some_and(|r| r == raw))
        .and_then(|ct| ct.targets.first())
        .map_or_else(|| node.id.clone(), |&sid| machine.node(sid).id.to_string())
}
