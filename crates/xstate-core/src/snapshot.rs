//! Snapshots (layout v4) and the machine structure hash, byte-compatible with the Python
//! library (ADR-0003): a snapshot written by one runtime restores in the other.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::SNAPSHOT_VERSION;
use crate::engine::Status;
use crate::error::{Error, Result};
use crate::event::names;
use crate::machine::{ActionDef, Guard, Machine, NodeKind, Trigger};
use crate::pyjson;

/// A persisted `after` timer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deadline {
    /// State whose `after` armed the timer.
    pub state_id: String,
    /// Sequence number of the state entry that armed it.
    pub entry_seq: u64,
    /// Wall-clock due time in epoch seconds.
    pub due_at_wall: f64,
    /// Declared delay in milliseconds.
    pub delay_ms: Value,
    /// Event the timer raises, in the Python / v4 spelling (`after.<key>.<state>`).
    pub event_type: String,
}

/// A persisted delayed `raise` / self-send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledSend {
    /// Always `"event"`.
    pub kind: String,
    /// Event type.
    #[serde(rename = "type")]
    pub ty: String,
    /// Event payload (`{}` when none).
    pub payload: Value,
    /// Time left when the snapshot was taken.
    pub remaining_ms: f64,
    /// Cancellation id, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_id: Option<String>,
}

/// Snapshot layout v4. Field order matches the Python writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Layout version (4).
    pub version: u32,
    /// Machine id.
    pub machine_id: String,
    /// [`machine_hash`] of the machine that produced it.
    pub machine_hash: String,
    /// Chart `version` label.
    pub machine_version: Option<String>,
    /// Epoch seconds when taken.
    pub taken_at: f64,
    /// Status.
    pub status: Status,
    /// Context.
    pub context: Value,
    /// Active leaf ids, sorted.
    pub state_ids: Vec<String>,
    /// XState-style hierarchical state value.
    pub value: Value,
    /// Every active state id (ancestors included), sorted.
    pub configuration: Vec<String>,
    /// Machine output once done.
    pub output: Value,
    /// Error message once errored.
    pub error: Option<String>,
    /// Number of `maxIterations` trips so far.
    pub chain_trips: u32,
    /// Last `maxIterations` error message.
    pub last_chain_error: Option<String>,
    /// Deferred events (`onUnhandled: defer`).
    pub deferred: Vec<Value>,
    /// Events queued but not yet processed.
    pub pending_events: Vec<Value>,
    /// Pending delayed self-sends.
    pub scheduled_sends: Vec<ScheduledSend>,
    /// Pending `after` timers.
    pub deadlines: Vec<Deadline>,
    /// Exited state id → active descendants at exit time.
    pub history: IndexMap<String, Vec<String>>,
    /// Child actors (recursive snapshots).
    pub actors: Map<String, Value>,
    /// `systemId` → actor id.
    pub system: Map<String, Value>,
}

impl Snapshot {
    /// Serialise to JSON text (the on-the-wire form stores expect).
    ///
    /// # Errors
    /// Never fails for a well-formed snapshot; propagates serialisation errors otherwise.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parse a snapshot of this or an older layout version.
    ///
    /// # Errors
    /// [`Error::Snapshot`] for a newer layout than this engine supports, for malformed
    /// documents and for anything larger than [`crate::DEFAULT_MAX_INPUT_BYTES`].
    pub fn from_json(text: &str) -> Result<Self> {
        if text.len() > crate::DEFAULT_MAX_INPUT_BYTES {
            return Err(Error::Snapshot(format!(
                "snapshot is {} bytes; limit is {}",
                text.len(),
                crate::DEFAULT_MAX_INPUT_BYTES
            )));
        }
        let mut raw: Value = serde_json::from_str(text)
            .map_err(|e| Error::Snapshot(format!("malformed JSON: {e}")))?;
        let version = match raw.get("version") {
            None => 0,
            Some(Value::Number(n)) => {
                n.as_u64().ok_or_else(|| Error::Snapshot("'version' is not an integer".into()))?
            }
            Some(other) => {
                return Err(Error::Snapshot(format!("'version' is {other}, expected an integer")));
            }
        };
        if version > u64::from(SNAPSHOT_VERSION) {
            return Err(Error::Snapshot(format!(
                "snapshot layout v{version} is newer than the supported v{SNAPSHOT_VERSION}"
            )));
        }
        upcast(&mut raw);
        serde_json::from_value(raw).map_err(|e| Error::Snapshot(format!("malformed snapshot: {e}")))
    }
}

/// Fill in the fields older layouts lacked so the v4 model deserialises.
fn upcast(raw: &mut Value) {
    let Some(obj) = raw.as_object_mut() else { return };
    let defaults: [(&str, Value); 14] = [
        ("version", Value::from(SNAPSHOT_VERSION)),
        ("machine_hash", Value::String(String::new())),
        ("machine_version", Value::Null),
        ("taken_at", Value::from(0.0)),
        ("value", Value::Null),
        ("configuration", Value::Array(Vec::new())),
        ("output", Value::Null),
        ("error", Value::Null),
        ("chain_trips", Value::from(0)),
        ("last_chain_error", Value::Null),
        ("deferred", Value::Array(Vec::new())),
        ("pending_events", Value::Array(Vec::new())),
        ("scheduled_sends", Value::Array(Vec::new())),
        ("deadlines", Value::Array(Vec::new())),
    ];
    for (key, value) in defaults {
        obj.entry(key).or_insert(value);
    }
    for key in ["history", "actors", "system"] {
        obj.entry(key).or_insert_with(|| Value::Object(Map::new()));
    }
    obj.insert("version".into(), Value::from(SNAPSHOT_VERSION));
}

// ----------------------------------------------------------------------------- hash

/// 16 hex characters of SHA-256 over the machine's behavioural structure, identical to the
/// Python library's `structure_hash`: states, transitions (event, target, guard shape, action
/// names, reenter), invokes and delays — never `meta`, descriptions, tags or params.
#[must_use]
pub fn machine_hash(machine: &Machine) -> String {
    let mut shapes: Vec<String> =
        machine.states().iter().enumerate().map(|(i, _)| node_shape(machine, i)).collect();
    shapes.sort();
    let digest = Sha256::digest(shapes.join("\n").as_bytes());
    hex::encode(digest)[..16].to_owned()
}

fn node_shape(machine: &Machine, index: usize) -> String {
    let node = &machine.states()[index];
    let sid = crate::StateId::from_index(index);
    let mut entry: Vec<Value> = node.entry.iter().map(|a| Value::String(action_name(a))).collect();
    let mut exit: Vec<Value> = node.exit.iter().map(|a| Value::String(action_name(a))).collect();
    sort_strings(&mut entry);
    sort_strings(&mut exit);
    let mut after: Vec<Value> = node.after.iter().map(|a| Value::String(a.key.clone())).collect();
    sort_strings(&mut after);
    let mut invoke: Vec<(String, String)> =
        node.invokes.iter().map(|i| (i.src.clone(), i.id.clone())).collect();
    invoke.sort();
    let mut transitions: Vec<String> = machine
        .transitions()
        .iter()
        .filter(|t| t.source == sid)
        .map(|t| {
            let event = match &t.trigger {
                Trigger::Always => String::new(),
                Trigger::Event(e) => python_event_name(e),
            };
            let target =
                t.raw_targets.first().map_or(pyjson::NONE.to_owned(), |s| pyjson::repr_str(s));
            let guard = pyjson::repr_str(&pyjson::dumps(&guard_shape(t.guard.as_ref())));
            let actions: Vec<String> =
                t.actions.iter().map(|a| pyjson::repr_str(&action_name(a))).collect();
            format!(
                "({}, {target}, {guard}, {}, {})",
                pyjson::repr_str(&event),
                pyjson::repr_tuple(&actions),
                if t.reenter { "True" } else { "False" }
            )
        })
        .collect();
    transitions.sort();
    let kind = match node.kind {
        NodeKind::Atomic => "atomic",
        NodeKind::Compound { .. } => "compound",
        NodeKind::Parallel => "parallel",
        NodeKind::Final => "final",
        NodeKind::History { .. } => "history",
    };
    let shape = serde_json::json!({
        "id": node.id,
        "type": kind,
        "initial": node.initial_key,
        "entry": entry,
        "exit": exit,
        "transitions": transitions,
        "invoke": invoke.into_iter().map(|(s, i)| Value::Array(vec![Value::String(s), Value::String(i)])).collect::<Vec<_>>(),
        "after": after,
    });
    pyjson::dumps(&shape)
}

fn sort_strings(values: &mut [Value]) {
    values.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
}

fn action_name(action: &ActionDef) -> String {
    match action {
        ActionDef::User { name, .. } | ActionDef::Builtin { name, .. } => name.clone(),
    }
}

fn guard_shape(guard: Option<&Guard>) -> Value {
    match guard {
        None => Value::Null,
        Some(Guard::Named { name, .. }) => serde_json::json!({ "type": name }),
        Some(Guard::StateIn { raw, .. }) => {
            serde_json::json!({ "type": "stateIn", "stateIn": raw })
        }
        Some(Guard::Not(inner)) => {
            serde_json::json!({ "type": "not", "children": [guard_shape(Some(inner))] })
        }
        Some(Guard::And(list)) => {
            serde_json::json!({ "type": "and", "children": list.iter().map(|g| guard_shape(Some(g))).collect::<Vec<_>>() })
        }
        Some(Guard::Or(list)) => {
            serde_json::json!({ "type": "or", "children": list.iter().map(|g| guard_shape(Some(g))).collect::<Vec<_>>() })
        }
    }
}

/// The Python / XState v4 spelling of an engine event name, as stored in snapshots and hashed.
#[must_use]
pub fn python_event_name(v5: &str) -> String {
    const BACK: [(&str, &str); 4] = [
        ("xstate.done.state.", "done.state."),
        ("xstate.done.actor.", "done.invoke."),
        ("xstate.error.actor.", "error.platform."),
        ("xstate.after.", "after."),
    ];
    for (new, old) in BACK {
        if let Some(rest) = v5.strip_prefix(new) {
            return format!("{old}{rest}");
        }
    }
    v5.to_owned()
}

/// Inverse of [`python_event_name`] (also accepts v5 names unchanged).
#[must_use]
pub fn engine_event_name(stored: &str) -> String {
    names::normalize(stored).into_owned()
}

/// XState-style hierarchical state value for a configuration.
pub(crate) fn state_value(
    machine: &Machine,
    active: &dyn Fn(crate::StateId) -> bool,
    node: crate::StateId,
) -> Value {
    let n = machine.node(node);
    let active_children: Vec<crate::StateId> =
        n.children.iter().copied().filter(|&c| active(c)).collect();
    match n.kind {
        NodeKind::Parallel => {
            let mut map = Map::new();
            for c in active_children {
                map.insert(machine.node(c).key.clone(), state_value(machine, active, c));
            }
            Value::Object(map)
        }
        NodeKind::Compound { .. } => match active_children.first() {
            Some(&c) if machine.node(c).is_leaf() => Value::String(machine.node(c).key.clone()),
            Some(&c) => {
                let mut map = Map::new();
                map.insert(machine.node(c).key.clone(), state_value(machine, active, c));
                Value::Object(map)
            }
            None => Value::Object(Map::new()),
        },
        _ => Value::Object(Map::new()),
    }
}

/// Validate that `ids` name existing states and form a legal configuration; returns them as
/// state ids.
pub(crate) fn resolve_configuration(
    machine: &Machine,
    ids: &[String],
) -> Result<Vec<crate::StateId>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let sid = machine.state_by_id(id).ok_or_else(|| Error::StateNotFound {
            target: id.clone(),
            from: "snapshot.configuration".into(),
        })?;
        out.push(sid);
    }
    let set: HashSet<crate::StateId> = out.iter().copied().collect();
    for &sid in &out {
        let node = machine.node(sid);
        if node.is_history() {
            return Err(Error::Snapshot(format!("history state '{}' cannot be active", node.id)));
        }
        if let Some(parent) = node.parent
            && !set.contains(&parent)
        {
            return Err(Error::Snapshot(format!("'{}' is active but its parent is not", node.id)));
        }
        let active_children = node.children.iter().filter(|c| set.contains(c)).count();
        match node.kind {
            NodeKind::Compound { .. } if active_children != 1 => {
                return Err(Error::Snapshot(format!(
                    "compound state '{}' must have exactly one active child",
                    node.id
                )));
            }
            NodeKind::Parallel
                if active_children
                    != node.children.iter().filter(|&&c| !machine.node(c).is_history()).count() =>
            {
                return Err(Error::Snapshot(format!(
                    "parallel state '{}' must have every region active",
                    node.id
                )));
            }
            _ => {}
        }
    }
    if !set.contains(&crate::StateId::ROOT) {
        return Err(Error::Snapshot("configuration does not include the root".into()));
    }
    Ok(out)
}
