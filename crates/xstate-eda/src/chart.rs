//! Chart helpers shared by the outbox, the dead-letter plugin and the AsyncAPI generator:
//! states entered by a step, and a walk over the chart's JSON document in Python's order.

use serde_json::{Map, Value};
use xstate_core::machine::{NodeKind, Transition};
use xstate_core::{ActiveState, Machine, StateId, TransitionId};

// ----------------------------------------------------------------------- entered states

/// The domain of a transition (SCXML): the source for an internal transition into its own
/// subtree, else the least common proper ancestor of source and targets. `None` when targetless.
fn domain(machine: &Machine, t: &Transition) -> Option<StateId> {
    let targets: Vec<StateId> = t
        .targets
        .iter()
        .map(|&x| match machine.node(x).kind {
            NodeKind::History { .. } => machine.node(x).parent.unwrap_or(StateId::ROOT),
            _ => x,
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    if !t.reenter && targets.iter().all(|&x| machine.is_self_or_descendant(x, t.source)) {
        return Some(t.source);
    }
    Some(
        machine
            .ancestors(t.source, None)
            .find(|&a| targets.iter().all(|&n| machine.is_descendant(n, a)))
            .unwrap_or(StateId::ROOT),
    )
}

/// For each transition taken by a step, the states it entered that are still active, sorted
/// by id, each state reported once per step.
///
/// The engine reports a whole macrostep at once, so "entered" is reconstructed: a state counts
/// when it is active after the step, lies strictly inside the transition's domain and is not
/// an ancestor-or-self of the transition's source (those were active before). This matches
/// Python's per-microstep `to_states - from_states` except for states exited again later in
/// the same macrostep (not reported) and states re-entered from outside the source path (e.g.
/// the children of a re-entered compound state), which are reported again.
pub(crate) fn entered_per_transition<C>(
    machine: &Machine,
    transitions: &[TransitionId],
    state: &ActiveState<C>,
) -> Vec<(TransitionId, Vec<StateId>)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(transitions.len());
    for &tid in transitions {
        let t = machine.transition(tid);
        let mut entered: Vec<StateId> = match domain(machine, t) {
            Some(d) => state
                .configuration()
                .filter(|&s| {
                    machine.is_descendant(s, d) && !machine.is_self_or_descendant(t.source, s)
                })
                .filter(|s| seen.insert(*s))
                .collect(),
            None => Vec::new(),
        };
        entered.sort_by(|a, b| machine.node(*a).id.cmp(&machine.node(*b).id));
        out.push((tid, entered));
    }
    out
}

/// Every active state sorted by id (what a start "enters").
pub(crate) fn active_sorted<C>(machine: &Machine, state: &ActiveState<C>) -> Vec<StateId> {
    let mut all: Vec<StateId> = state.configuration().collect();
    all.sort_by(|a, b| machine.node(*a).id.cmp(&machine.node(*b).id));
    all
}

/// `true` when the compiled state carries `tag`.
pub(crate) fn has_tag(machine: &Machine, sid: StateId, tag: &str) -> bool {
    machine.node(sid).tags.iter().any(|t| t == tag)
}

/// The state key without the `<machine id>.` prefix.
pub(crate) fn state_key<'a>(id: &'a str, machine_id: &str) -> &'a str {
    id.strip_prefix(machine_id).and_then(|rest| rest.strip_prefix('.')).unwrap_or(id)
}

// --------------------------------------------------------------------------- JSON walk

/// One state of the chart document, in Python's `walk()` (pre-)order.
pub(crate) struct ChartNode<'a> {
    /// Dotted id (`machine.a.b`), as Python builds it.
    pub id: String,
    /// The state's configuration object.
    pub config: &'a Map<String, Value>,
}

/// Every state of `doc` (the machine's [`Machine::to_json`]) depth-first, root first.
pub(crate) fn walk<'a>(doc: &'a Value, machine_id: &str) -> Vec<ChartNode<'a>> {
    let mut out = Vec::new();
    if let Value::Object(root) = doc {
        visit(root, machine_id, &mut out);
    }
    out
}

fn visit<'a>(config: &'a Map<String, Value>, id: &str, out: &mut Vec<ChartNode<'a>>) {
    let children = config.get("states").and_then(Value::as_object);
    out.push(ChartNode { id: id.to_owned(), config });
    for (key, child) in children.into_iter().flatten() {
        if let Value::Object(child) = child {
            visit(child, &format!("{id}.{key}"), out);
        }
    }
}

/// Python `_normalize_transitions`: a transition value as a list of transition objects
/// (`null` = forbidden, which has no `meta`).
pub(crate) fn normalize_transitions(value: &Value) -> Vec<Map<String, Value>> {
    let target = |s: &str| {
        let mut m = Map::new();
        m.insert("target".into(), Value::from(s));
        m
    };
    match value {
        Value::String(s) => vec![target(s)],
        Value::Object(m) => vec![m.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(target(s)),
                Value::Object(m) => Some(m.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Python `_all_transitions(node)`: `(event label, transitions)` rows: `on` (with `always`
/// merged into the `""` bucket), `after`, every invoke's `onDone` / `onError`, then `onDone`.
pub(crate) fn transition_rows(node: &ChartNode<'_>) -> Vec<(String, Vec<Map<String, Value>>)> {
    let cfg = node.config;
    let mut rows: Vec<(String, Vec<Map<String, Value>>)> = cfg
        .get("on")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(ev, v)| (ev.clone(), normalize_transitions(v)))
        .collect();
    if let Some(always) = cfg.get("always").filter(|v| !v.is_null()) {
        let extra = normalize_transitions(always);
        match rows.iter_mut().find(|(ev, _)| ev.is_empty()) {
            Some((_, ts)) => ts.extend(extra),
            None => rows.push((String::new(), extra)),
        }
    }
    for (delay, v) in cfg.get("after").and_then(Value::as_object).into_iter().flatten() {
        let key = delay.trim().parse::<i64>().map_or_else(|_| delay.clone(), |n| n.to_string());
        rows.push((format!("after.{key}"), normalize_transitions(v)));
    }
    rows.extend(invoke_rows(node));
    if let Some(on_done) = cfg.get("onDone").filter(|v| !crate::py::is_falsy(v))
        && let Some(first) = normalize_transitions(on_done).into_iter().next()
    {
        rows.push((format!("done.state.{}", node.id), vec![first]));
    }
    rows
}

fn invoke_rows(node: &ChartNode<'_>) -> Vec<(String, Vec<Map<String, Value>>)> {
    let invokes: Vec<&Value> = match node.config.get("invoke") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(single) => vec![single],
    };
    let mut rows = Vec::new();
    for inv in invokes.into_iter().filter_map(Value::as_object) {
        let id = match inv.get("id") {
            None => node.id.clone(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => crate::py::repr_value(other),
        };
        let list = |key: &str| inv.get(key).map(normalize_transitions).unwrap_or_default();
        rows.push((format!("done.invoke.{id}"), list("onDone")));
        rows.push((format!("error.platform.{id}"), list("onError")));
    }
    rows
}

/// The tags of a state configuration (`"tags"` as a string or a list).
pub(crate) fn config_has_tag(config: &Map<String, Value>, tag: &str) -> bool {
    match config.get("tags") {
        Some(Value::String(s)) => s == tag,
        Some(Value::Array(items)) => items.iter().any(|t| t.as_str() == Some(tag)),
        _ => false,
    }
}
