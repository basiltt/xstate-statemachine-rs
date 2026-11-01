//! Adopt an existing record into a machine: "this row is already in state X".
//!
//! [`from_state_ids`] builds the layout-v4 [`Snapshot`] a machine that had reached a given
//! configuration would have written, **without running anything**: no entry actions, no
//! services, no timers are armed. It is the generic primitive behind data migrations from a
//! status column (`django-fsm`, a SQL enum, a plain dict). Ports Python `persistence/adopt.py`.
//!
//! Given ids are completed downwards: a compound state without an explicit child enters its
//! `initial`; every region of a parallel state is entered (its given child, else its
//! `initial`). Ancestors are implied. The result is checked for SCXML legality (one leaf per
//! region, no history ids) and must restore through [`SyncInterpreter::from_snapshot`], so a
//! snapshot the engine would refuse is never returned.
//!
//! `xstate_core::ActiveState` cannot be built from outside the core crate, so the [`Snapshot`]
//! is assembled directly (its fields are public); the `configuration` and `value` are computed
//! here with the same rules as the core's `Snapshot::from_state`.
//!
//! Restore validation uses an empty logic registry. When the machine names user actions, guards
//! or services the registry cannot satisfy, the restore stops at the missing implementation,
//! *after* the configuration was already validated; that outcome counts as success.
//!
//! # Not ported
//!
//! Named delays (`after: {"BACKOFF": ...}`) are skipped when `timers` is requested: resolving
//! them needs the logic registry. Only fixed numeric delays get a deadline.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};
use xstate_core::machine::{Delay, NodeKind};
use xstate_core::snapshot::{Deadline, Snapshot, python_event_name};
use xstate_core::{
    DynContext, Error, Machine, MachineLogic, RealClock, RestartTimers, RestoreOptions, StateId,
    Status, SyncInterpreter,
};

use crate::store::Result;

/// Delays beyond this (milliseconds) cannot be stored exactly as an integer; they are skipped.
const MAX_DELAY_MS: f64 = 9.0e15;

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidConfig { path: "from_state_ids".into(), message: message.into() }
}

fn resolve(machine: &Machine, id: &str) -> std::result::Result<StateId, Error> {
    let found = machine.state_by_id(id).or_else(|| {
        let prefix = format!("{}.", machine.id());
        (!id.starts_with(&prefix)).then(|| machine.state_by_id(&format!("{prefix}{id}"))).flatten()
    });
    found.ok_or_else(|| Error::StateNotFound {
        target: id.to_owned(),
        from: "from_state_ids".into(),
    })
}

fn has_chosen(machine: &Machine, node: StateId, chosen: &HashSet<StateId>) -> bool {
    chosen.iter().any(|&c| machine.is_self_or_descendant(c, node))
}

/// Add `node` and the descendants a machine entering it would have.
fn complete(
    machine: &Machine,
    node: StateId,
    chosen: &HashSet<StateId>,
    out: &mut BTreeSet<StateId>,
) -> std::result::Result<(), Error> {
    out.insert(node);
    let n = machine.node(node);
    let children: Vec<StateId> =
        n.children.iter().copied().filter(|&c| !machine.node(c).is_history()).collect();
    match n.kind {
        NodeKind::Parallel => {
            for region in children {
                complete(machine, region, chosen, out)?;
            }
            Ok(())
        }
        NodeKind::Compound { initial } => {
            let picked: Vec<StateId> =
                children.into_iter().filter(|&c| has_chosen(machine, c, chosen)).collect();
            match picked.as_slice() {
                [] => complete(machine, initial, chosen, out),
                [one] => complete(machine, *one, chosen, out),
                many => {
                    let mut ids: Vec<&str> = many.iter().map(|&c| &*machine.node(c).id).collect();
                    ids.sort_unstable();
                    Err(invalid(format!(
                        "'{}' is a compound state but {ids:?} were all given; exactly one child \
                         can be active.",
                        n.id
                    )))
                }
            }
        }
        NodeKind::Atomic | NodeKind::Final | NodeKind::History { .. } => Ok(()),
    }
}

/// XState-style hierarchical state value of a configuration (same rules as the core).
fn state_value(machine: &Machine, active: &BTreeSet<StateId>, node: StateId) -> Value {
    let n = machine.node(node);
    let mut kids = n.children.iter().copied().filter(|c| active.contains(c));
    match n.kind {
        NodeKind::Parallel => Value::Object(
            kids.map(|c| (machine.node(c).key.to_string(), state_value(machine, active, c)))
                .collect(),
        ),
        NodeKind::Compound { .. } => match kids.next() {
            Some(c) if machine.node(c).is_leaf() => Value::String(machine.node(c).key.to_string()),
            Some(c) => {
                let mut map = Map::new();
                map.insert(machine.node(c).key.to_string(), state_value(machine, active, c));
                Value::Object(map)
            }
            None => Value::Object(Map::new()),
        },
        _ => Value::Object(Map::new()),
    }
}

/// One deadline per fixed `after` delay of the active states, anchored at `now_wall`: what
/// entering them would have armed. `entry_seq` is synthetic (1, 2, ... in state-id order).
fn deadlines(machine: &Machine, active: &BTreeSet<StateId>, now_wall: f64) -> Vec<Deadline> {
    let mut nodes: Vec<StateId> = active.iter().copied().collect();
    nodes.sort_by(|a, b| machine.node(*a).id.cmp(&machine.node(*b).id));
    let mut out = Vec::new();
    for (index, &sid) in nodes.iter().enumerate() {
        let node = machine.node(sid);
        for after in &node.after {
            let Delay::Fixed(ms) = after.delay else { continue };
            if !ms.is_finite() || ms > MAX_DELAY_MS {
                continue;
            }
            let ms = ms.max(0.0);
            #[allow(clippy::cast_possible_truncation)]
            let delay_ms = Value::from(ms.round_ties_even() as i64);
            out.push(Deadline {
                state_id: node.id.to_string(),
                entry_seq: index as u64 + 1,
                due_at_wall: now_wall + ms / 1000.0,
                delay_ms,
                event_type: python_event_name(&format!("xstate.after.{}.{}", after.key, node.id)),
            });
        }
    }
    out.sort_by(|a, b| {
        a.due_at_wall.total_cmp(&b.due_at_wall).then_with(|| a.state_id.cmp(&b.state_id))
    });
    out
}

/// The initial context merged with `context` (keys given win), like a restore.
fn merged_context(machine: &Machine, context: Value) -> std::result::Result<Value, Error> {
    let mut base = match machine.initial_context() {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    match context {
        Value::Null => {}
        Value::Object(map) => base.extend(map),
        other => return Err(invalid(format!("context must be a JSON object, got {other}"))),
    }
    Ok(Value::Object(base))
}

/// The layout-v4 snapshot of `machine` in the configuration `state_ids`.
///
/// * `state_ids`: full ids (`"order.review.legal.ok"`), custom `#id`s (without `#`) or ids
///   relative to the root (`"review.legal.ok"`); leaves or ancestors. See the module notes for
///   how gaps are completed.
/// * `context`: a JSON object merged over the machine's initial context (`null` = none).
/// * `status`: [`Status::Running`] or [`Status::Done`] (for a final state).
/// * `timers`: also record one [`Deadline`] per fixed `after` delay of the adopted
///   configuration, due `delay` after `now_wall`: adoption counts as entry, because the
///   record's real entry time is unknown. Without it nothing is recorded and the timer starts
///   when the machine is next started with `restart_timers`.
/// * `now_wall`: epoch seconds stamped as `taken_at` and anchoring the deadlines.
///
/// # Errors
/// [`StoreError::Engine`](crate::StoreError::Engine) wrapping [`Error::StateNotFound`] for an
/// unknown id, or [`Error::InvalidConfig`] for no ids, a bad status or context, a history id, two
/// children of one compound state, or a compound state without `initial` and no child given.
pub fn from_state_ids(
    machine: &Machine,
    state_ids: &[&str],
    context: Value,
    status: Status,
    timers: bool,
    now_wall: f64,
) -> Result<Snapshot> {
    if state_ids.is_empty() {
        return Err(invalid("no state ids given").into());
    }
    if !matches!(status, Status::Running | Status::Done) {
        return Err(invalid("status must be 'running' or 'done'").into());
    }
    let mut chosen = HashSet::new();
    for id in state_ids {
        let sid = resolve(machine, id)?;
        if machine.node(sid).is_history() {
            return Err(invalid(format!("'{id}' is a history state and cannot be active")).into());
        }
        chosen.insert(sid);
    }
    let mut active = BTreeSet::new();
    complete(machine, StateId::ROOT, &chosen, &mut active)?;
    let mut conflicting: Vec<&str> =
        chosen.iter().filter(|c| !active.contains(c)).map(|&c| &*machine.node(c).id).collect();
    if !conflicting.is_empty() {
        conflicting.sort_unstable();
        return Err(invalid(format!(
            "{conflicting:?} conflict with the rest of the configuration (different branches of \
             one compound state)."
        ))
        .into());
    }
    let mut leaves: Vec<String> = active
        .iter()
        .map(|&s| machine.node(s))
        .filter(|n| n.children.iter().all(|&c| machine.node(c).is_history()))
        .map(|n| n.id.to_string())
        .collect();
    leaves.sort();
    let mut configuration: Vec<String> =
        active.iter().map(|&s| machine.node(s).id.to_string()).collect();
    configuration.sort();
    let snapshot = Snapshot {
        version: xstate_core::SNAPSHOT_VERSION,
        machine_id: machine.id().to_owned(),
        machine_hash: machine.structure_hash().to_owned(),
        machine_version: machine.version().map(str::to_owned),
        taken_at: now_wall,
        status,
        context: merged_context(machine, context)?,
        state_ids: leaves,
        value: state_value(machine, &active, StateId::ROOT),
        configuration,
        output: Value::Null,
        error: None,
        chain_trips: 0,
        last_chain_error: None,
        deferred: Vec::new(),
        pending_events: Vec::new(),
        scheduled_sends: Vec::new(),
        deadlines: if timers { deadlines(machine, &active, now_wall) } else { Vec::new() },
        history: std::iter::empty().collect(),
        actors: Map::new(),
        system: Map::new(),
    };
    validate_restore(machine, &snapshot)?;
    Ok(snapshot)
}

/// Round-trip through the engine's own restore: shape, identity, ids and legality.
fn validate_restore(machine: &Machine, snapshot: &Snapshot) -> Result<()> {
    let options = RestoreOptions { verify_hash: Some(true), restart_timers: RestartTimers::Parked };
    match SyncInterpreter::<DynContext>::from_snapshot(
        machine.clone(),
        MachineLogic::dynamic(),
        snapshot,
        Arc::new(RealClock::new()),
        &options,
    ) {
        Ok(_) | Err(Error::ImplementationMissing { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
