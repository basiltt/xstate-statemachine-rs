//! Direct compilation of a JSON value into the compiled arena, without the owned
//! [`MachineConfig`](crate::config::MachineConfig) in between.
//!
//! `Machine::from_value` first tries this path. It reads the document in one pass and hands
//! the pieces to the same [`Compiler`] the configuration path uses (target resolution, event
//! interning, transition tables, validation and assembly are shared), so a machine built here
//! is identical to one built through the parser. It accepts only documents it can compile
//! exactly as the parser would *with no warnings*: plain states (`id`, `initial`, `states`,
//! `type`, `on`, `always`, `after`, `entry`, `exit`, `description`), transitions written as
//! target strings or objects (`target`, `guard` by name, `actions` by name, `reenter`,
//! `description`), user actions only, and a root with at most `context` and `version` besides.
//! Anything else -- a built-in action, an action or guard object, `invoke`, `onDone`,
//! history states, policies, `x-` keys, a key it does not know, and every malformed or
//! unresolvable document -- makes it return `None`, and the caller takes the parser path,
//! which produces the warnings and the exact error. `machine::direct_tests` builds every
//! conformance case and fixture both ways and compares the results.

use std::sync::Arc;

use serde_json::{Map, Value};
use smallvec::SmallVec;

use super::assemble::{self, Parts};
use super::builtin::{self, Delay};
use super::compile::{self, Compiler, Handlers, Skel};
use super::{ActionDef, AfterDef, Guard, MachineData, NodeKind, StateNode, Transition, Trigger};
use crate::config::{Policies, field};
use crate::event::names;
use crate::ids::{StateId, TransitionId};

/// State keys the direct path compiles.
const STATE_KEYS: &[&str] =
    &["id", "initial", "states", "type", "on", "always", "after", "entry", "exit", "description"];
/// Root-only keys the direct path reads.
const ROOT_KEYS: &[&str] = &["context", "version"];
/// Transition object keys the direct path compiles.
const TRANSITION_KEYS: &[&str] = &["target", "actions", "guard", "reenter", "description"];

/// Room reserved in the full-id buffer beyond the machine id.
const ID_HEADROOM: usize = 48;

/// What the first pass learned about one state.
struct Node<'v> {
    obj: &'v Map<String, Value>,
    custom_id: Option<&'v str>,
    kind: Kind,
    initial: Option<&'v str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Atomic,
    Compound,
    Parallel,
    Final,
}

/// Compile `value`, or `None` when the parser path must handle it. The machine's source is
/// left for the caller to set.
pub(super) fn compile(value: &Value) -> Option<MachineData> {
    let root = value.as_object()?;
    let machine_id = match field(root, "id") {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        _ => return None,
    };
    field(root, "states")?;
    let context = match field(root, "context") {
        None => Value::Null,
        Some(Value::String(_)) => return None,
        Some(v) => v.clone(),
    };
    let version = match field(root, "version") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return None,
    };
    let mut nodes: SmallVec<[Node<'_>; 16]> = SmallVec::new();
    let mut skels: SmallVec<[Skel<'_>; 16]> = SmallVec::new();
    let mut transitions = 0;
    // The full-id buffer, sized once for typical nesting; it ends up holding the machine id.
    let mut id = String::with_capacity(machine_id.len() + ID_HEADROOM);
    id.push_str(machine_id);
    let mut flat = Flat { nodes: &mut nodes, skels: &mut skels, transitions: &mut transitions };
    flat.state(root, machine_id, &mut id, None, 0)?;
    let (ids, custom_ids) = assemble::index_ids(&skels, |i| nodes[i].custom_id).ok()?;
    let children = assemble::index_children(&skels);
    let policies = Policies::default();
    let mut cx = Compiler::new(&skels, (&ids, &children, &custom_ids), machine_id, &policies, None);
    cx.reserve_transitions(transitions);
    let mut states: Vec<StateNode> = Vec::with_capacity(skels.len());
    for (i, (node, skel)) in nodes.iter().zip(&skels).enumerate() {
        states.push(compile_state(&mut cx, StateId::from_index(i), skel, node)?);
    }
    let finished = compile::finish(cx, states).ok()?;
    let source = super::source::Source::Document(Value::Null);
    let parts = Parts { id, context, version, policies, source };
    Some(assemble::assemble(finished, ids, custom_ids, parts))
}

/// The first pass: states in document (pre-)order, with their kinds checked.
struct Flat<'a, 'v> {
    nodes: &'a mut SmallVec<[Node<'v>; 16]>,
    skels: &'a mut SmallVec<[Skel<'v>; 16]>,
    /// Number of transitions declared (to size the table once).
    transitions: &'a mut usize,
}

impl<'v> Flat<'_, 'v> {
    /// Append the state `obj` (full id in `id`) and its subtree.
    fn state(
        &mut self,
        obj: &'v Map<String, Value>,
        key: &'v str,
        id: &mut String,
        parent: Option<StateId>,
        depth: u32,
    ) -> Option<()> {
        let is_root = parent.is_none();
        let known = |k: &String| {
            STATE_KEYS.contains(&k.as_str()) || (is_root && ROOT_KEYS.contains(&k.as_str()))
        };
        if !obj.keys().all(known) {
            return None;
        }
        let custom_id = match field(obj, "id") {
            _ if is_root => None,
            None => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
            Some(_) => return None,
        };
        let children = match field(obj, "states") {
            None => None,
            Some(Value::Object(map)) => Some(map),
            Some(_) => return None,
        };
        let kind = kind_of(obj, children.is_some_and(|m| !m.is_empty()))?;
        let initial = initial_of(obj, kind, children)?;
        *self.transitions += handler_count(obj)?;
        let me = StateId::from_index(self.skels.len());
        self.nodes.push(Node { obj, custom_id, kind, initial });
        self.skels.push(Skel { key, id: Arc::from(id.as_str()), parent, depth });
        for (child_key, child) in children.into_iter().flatten() {
            let Value::Object(child) = child else { return None };
            if child_key.is_empty() {
                return None;
            }
            let len = id.len();
            id.push('.');
            id.push_str(child_key);
            self.state(child, child_key, id, Some(me), depth + 1)?;
            id.truncate(len);
        }
        Some(())
    }
}

/// The state's kind, as the parser derives it; `None` for history states and for a `type`
/// that contradicts the children (an error or a warning on the parser path).
fn kind_of(obj: &Map<String, Value>, has_children: bool) -> Option<Kind> {
    match field(obj, "type") {
        None | Some(Value::Null) => Some(if has_children { Kind::Compound } else { Kind::Atomic }),
        Some(Value::String(ty)) => match (ty.as_str(), has_children) {
            ("compound", true) => Some(Kind::Compound),
            ("parallel", true) => Some(Kind::Parallel),
            ("atomic", false) => Some(Kind::Atomic),
            ("final", false) => Some(Kind::Final),
            _ => None,
        },
        Some(_) => None,
    }
}

/// `initial` as written, or inferred for a compound state with a single child; a compound
/// state's `initial` must name a child. `Some(None)` when there is none (not compound).
#[allow(clippy::option_option)] // see the comment in the body
fn initial_of<'v>(
    obj: &'v Map<String, Value>,
    kind: Kind,
    children: Option<&'v Map<String, Value>>,
) -> Option<Option<&'v str>> {
    // `None`: leave the state to the parser path; `Some(None)`: no `initial`.
    let written = match field(obj, "initial") {
        None => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return None,
    };
    if kind != Kind::Compound {
        return Some(written);
    }
    let children = children?;
    match written {
        Some(key) => children.keys().any(|k| k == key).then_some(Some(key)),
        // No history states on this path: a single child is the only non-history child.
        None if children.len() == 1 => children.keys().next().map(|k| Some(k.as_str())),
        None => None,
    }
}

/// Number of transitions the state's `on`, `after` and `always` declare.
fn handler_count(obj: &Map<String, Value>) -> Option<usize> {
    let arity = |v: &Value| match v {
        Value::Array(items) => items.len(),
        _ => 1,
    };
    let mut count = 0;
    for key in ["on", "after"] {
        match field(obj, key) {
            None => {}
            Some(Value::Object(map)) => count += map.values().map(arity).sum::<usize>(),
            Some(_) => return None,
        }
    }
    Some(count + field(obj, "always").map_or(0, arity))
}

/// The second pass over one state: handlers, actions and the compiled node.
fn compile_state(
    cx: &mut Compiler<'_, '_>,
    sid: StateId,
    skel: &Skel<'_>,
    node: &Node<'_>,
) -> Option<StateNode> {
    let obj = node.obj;
    let kind = match node.kind {
        Kind::Atomic => NodeKind::Atomic,
        Kind::Final => NodeKind::Final,
        Kind::Parallel => NodeKind::Parallel,
        Kind::Compound => NodeKind::Compound { initial: cx.child_by_key(sid, node.initial?)? },
    };
    let mut handlers = Handlers::default();
    for (descriptor, list) in field(obj, "on").and_then(Value::as_object).into_iter().flatten() {
        if descriptor.is_empty() {
            // The legacy spelling of `always`.
            return None;
        }
        add_event(cx, sid, &skel.id, descriptor, list, &mut handlers)?;
    }
    if let Some(after) = field(obj, "after").and_then(Value::as_object) {
        compile_after(cx, sid, &skel.id, after, &mut handlers)?;
    }
    if let Some(list) = field(obj, "always") {
        let compiled = transitions(cx, sid, &Trigger::Always, list, &skel.id)?;
        cx.has_always |= !compiled.is_empty();
        handlers.always.extend(compiled);
    }
    let entry = actions(field(obj, "entry"))?;
    let exit = actions(field(obj, "exit"))?;
    let initial_key = match kind {
        NodeKind::Compound { .. } => node.initial.map(smol_str::SmolStr::new),
        _ => None,
    };
    let description = match field(obj, "description") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let meta = compile::NodeMeta { description, ..compile::NodeMeta::default() };
    Some(compile::state_node(skel, kind, handlers, (entry, exit), initial_key, meta))
}

/// Delayed transitions: ordinary transitions on their (normalised) timer event.
fn compile_after(
    cx: &mut Compiler<'_, '_>,
    sid: StateId,
    id: &str,
    after: &Map<String, Value>,
    out: &mut Handlers,
) -> Option<()> {
    for (raw_key, list) in after {
        let (delay, key) = Delay::parse_after_key(raw_key).ok()?;
        if out.after.iter().any(|a| a.key == key) {
            return None;
        }
        let event = names::after(&key, id);
        let ty = add_event(cx, sid, id, &event, list, out)?;
        out.after_events.push(ty);
        out.after.push(AfterDef { key, delay, event, raw_key: raw_key.clone() });
    }
    Some(())
}

/// Compile the transitions of one event descriptor and file them under `out`.
fn add_event(
    cx: &mut Compiler<'_, '_>,
    sid: StateId,
    path: &str,
    descriptor: &str,
    list: &Value,
    out: &mut Handlers,
) -> Option<smol_str::SmolStr> {
    let descriptor = smol_str::SmolStr::new(names::normalize(descriptor));
    let compiled = transitions(cx, sid, &Trigger::Event(descriptor.clone()), list, path)?;
    cx.file_event(&descriptor, compiled, out).ok()?;
    Some(descriptor)
}

/// A transition list: `null` (forbidden), one transition, or an array of them.
fn transitions(
    cx: &mut Compiler<'_, '_>,
    sid: StateId,
    trigger: &Trigger,
    value: &Value,
    path: &str,
) -> Option<SmallVec<[TransitionId; 2]>> {
    let mut out = SmallVec::new();
    match value {
        Value::Null => out.push(cx.push_transition(forbidden(sid, trigger))),
        Value::Array(items) => {
            for item in items {
                out.push(transition(cx, sid, trigger, item, path)?);
            }
        }
        Value::String(_) | Value::Object(_) => out.push(transition(cx, sid, trigger, value, path)?),
        _ => return None,
    }
    Some(out)
}

/// The forbidden transition (`"E": null`, `"E": {}`): consumes the event, does nothing.
fn forbidden(source: StateId, trigger: &Trigger) -> Transition {
    Transition {
        source,
        trigger: trigger.clone(),
        targets: Vec::new(),
        raw_targets: SmallVec::new(),
        actions: Vec::new(),
        guard: None,
        reenter: false,
        forbidden: true,
        meta: None,
        description: None,
    }
}

/// One transition item: a target string or a transition object.
fn transition(
    cx: &mut Compiler<'_, '_>,
    source: StateId,
    trigger: &Trigger,
    item: &Value,
    path: &str,
) -> Option<TransitionId> {
    let obj = match item {
        Value::String(t) if !t.is_empty() => return target_only(cx, source, trigger, t, path),
        Value::Object(obj) => obj,
        _ => return None,
    };
    if obj.is_empty() {
        return Some(cx.push_transition(forbidden(source, trigger)));
    }
    if !obj.keys().all(|k| TRANSITION_KEYS.contains(&k.as_str())) {
        return None;
    }
    let written = targets(field(obj, "target"))?;
    let targets = cx.resolve_targets(source, &written, path).ok()?;
    let guard = match field(obj, "guard") {
        None => None,
        Some(Value::String(name)) => Some(guard(name)?),
        Some(_) => return None,
    };
    let reenter = match field(obj, "reenter") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return None,
    };
    let description = match field(obj, "description") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(cx.push_transition(Transition {
        source,
        trigger: trigger.clone(),
        targets,
        raw_targets: written.iter().map(smol_str::SmolStr::new).collect(),
        actions: actions(field(obj, "actions"))?,
        guard,
        reenter,
        forbidden: false,
        meta: None,
        description,
    }))
}

/// A transition written as its target string.
fn target_only(
    cx: &mut Compiler<'_, '_>,
    source: StateId,
    trigger: &Trigger,
    target: &str,
    path: &str,
) -> Option<TransitionId> {
    let targets = cx.resolve_targets(source, &[target], path).ok()?;
    let raw_targets = SmallVec::from_elem(smol_str::SmolStr::new(target), 1);
    let plain = Transition { targets, raw_targets, forbidden: false, ..forbidden(source, trigger) };
    Some(cx.push_transition(plain))
}

/// `target`: absent / `null`, a non-empty string, or an array of them.
fn targets(value: Option<&Value>) -> Option<SmallVec<[&str; 1]>> {
    fn non_empty(v: &Value) -> Option<&str> {
        v.as_str().filter(|s| !s.is_empty())
    }
    match value {
        None | Some(Value::Null) => Some(SmallVec::new()),
        Some(Value::String(_)) => value.and_then(non_empty).map(|t| SmallVec::from_elem(t, 1)),
        Some(Value::Array(items)) => items.iter().map(non_empty).collect(),
        Some(_) => None,
    }
}

/// A guard by name (`"!name"` negates); `stateIn` needs a state argument the string form
/// cannot carry, so it is left to the parser path.
fn guard(name: &str) -> Option<Guard> {
    let named = |name: &str| {
        (!name.is_empty() && name != "stateIn")
            .then(|| Guard::Named { name: name.to_owned(), params: Value::Null })
    };
    match name.strip_prefix('!') {
        Some(inner) => named(inner).map(|g| Guard::Not(Box::new(g))),
        None => named(name),
    }
}

/// An action list of user actions by name: absent / `null`, one name, or an array of names.
/// A built-in, an action object and an empty name are left to the parser path.
fn actions(value: Option<&Value>) -> Option<Vec<ActionDef>> {
    let user = |v: &Value| {
        let name = v.as_str().filter(|n| !n.is_empty() && builtin::canonical(n).is_none())?;
        Some(ActionDef::User { name: Arc::from(name), params: Value::Null })
    };
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(user).collect(),
        Some(single) => user(single).map(|a| vec![a]),
    }
}
