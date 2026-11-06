//! Assembly of a compiled machine from its parts, shared by the compiler of parsed
//! configurations (`super::compile`) and the direct compiler of JSON values (`super::direct`).

use std::collections::HashMap;
use std::sync::Arc;

use foldhash::HashMap as FastMap;
use serde_json::Value;
use smallvec::SmallVec;

use super::compile::Skel;
use super::{MachineData, StateNode, Transition};
use crate::config::ConfigWarning;
use crate::error::{Error, Result};
use crate::ids::StateId;

/// The pieces of a machine that do not come from its states.
pub(super) struct Parts {
    pub(super) id: String,
    pub(super) context: Value,
    pub(super) version: Option<String>,
    pub(super) policies: crate::config::Policies,
    pub(super) source: super::source::Source,
}

/// The full-id index and the custom-id index of a machine.
pub(super) type IdIndexes = (FastMap<Arc<str>, StateId>, HashMap<String, StateId>);

/// The full-id index (every state) and the custom-id index (`#id`; a duplicate is an error).
pub(super) fn index_ids<'c>(
    skels: &[Skel<'c>],
    custom: impl Fn(usize) -> Option<&'c str>,
) -> Result<IdIndexes> {
    let mut ids: FastMap<Arc<str>, StateId> = FastMap::default();
    ids.reserve(skels.len());
    let mut custom_ids: HashMap<String, StateId> = HashMap::new();
    for (i, skel) in skels.iter().enumerate() {
        let sid = StateId::from_index(i);
        ids.insert(Arc::clone(&skel.id), sid);
        if let Some(custom) = custom(i)
            && custom_ids.insert(custom.to_owned(), sid).is_some()
        {
            return Err(Error::config(&*skel.id, format!("duplicate custom id `#{custom}`")));
        }
    }
    Ok((ids, custom_ids))
}

/// Small machines find a child by scanning the flattened states; larger ones index them.
pub(super) fn index_children<'c>(skels: &[Skel<'c>]) -> FastMap<(StateId, &'c str), StateId> {
    let mut children: FastMap<(StateId, &str), StateId> = FastMap::default();
    if skels.len() > CHILD_SCAN_LIMIT {
        children.reserve(skels.len());
        for (i, skel) in skels.iter().enumerate() {
            if let Some(parent) = skel.parent {
                children.insert((parent, skel.key), StateId::from_index(i));
            }
        }
    }
    children
}

/// Up to this many states, children are looked up by scanning instead of through an index.
const CHILD_SCAN_LIMIT: usize = 32;

/// The compiled states and transitions, complete and validated.
pub(super) struct Finished {
    pub(super) states: Vec<StateNode>,
    pub(super) transitions: Vec<Transition>,
    pub(super) warnings: Vec<ConfigWarning>,
    pub(super) has_always: bool,
    pub(super) event_index: FastMap<smol_str::SmolStr, u32>,
    pub(super) event_names: SmallVec<[smol_str::SmolStr; 4]>,
}

/// The machine from its finished states and the rest of the document.
pub(super) fn assemble(
    finished: Finished,
    ids: FastMap<Arc<str>, StateId>,
    custom_ids: HashMap<String, StateId>,
    parts: Parts,
) -> MachineData {
    let Finished { states, transitions, warnings, has_always, event_index, event_names } = finished;
    let mut leaf_mask = crate::engine::bits::StateBits::with_capacity(states.len());
    for (i, node) in states.iter().enumerate() {
        leaf_mask.set(i, node.is_leaf());
    }
    let has_history = states.iter().any(StateNode::is_history);
    let has_parallel = states.iter().any(StateNode::is_parallel);
    let references_logic = states.iter().any(|s| {
        !s.entry.is_empty() || !s.exit.is_empty() || !s.invokes.is_empty() || !s.after.is_empty()
    }) || transitions
        .iter()
        .any(|t| !t.actions.is_empty() || t.guard.is_some());
    MachineData {
        id: parts.id,
        states,
        transitions,
        ids,
        custom_ids,
        context: parts.context,
        version: parts.version,
        policies: parts.policies,
        warnings,
        has_always,
        has_history,
        has_parallel,
        references_logic,
        leaf_mask,
        plans: std::sync::OnceLock::new(),
        initial_entry: std::sync::OnceLock::new(),
        declared_events: std::sync::OnceLock::new(),
        event_index,
        event_names,
        hash: std::sync::OnceLock::new(),
        source: parts.source,
    }
}

/// Children, subtree ends and history children, from the flattened (pre-)order.
pub(super) fn link_children(skels: &[Skel<'_>], states: &mut [StateNode]) -> Result<()> {
    for i in (0..skels.len()).rev() {
        let sid = StateId::from_index(i);
        let mut end = i + 1;
        for (j, r) in skels.iter().enumerate().skip(i + 1) {
            if r.depth <= skels[i].depth {
                break;
            }
            end = j + 1;
        }
        states[i].subtree_end =
            u32::try_from(end).map_err(|_| Error::LimitExceeded("too many states".into()))?;
        if let Some(parent) = skels[i].parent {
            states[parent.index()].children.insert(0, sid);
            if states[i].is_history() {
                states[parent.index()].history_children.insert(0, sid);
            }
        }
    }
    Ok(())
}

/// Record on every state which exact event keys its subtree handles, and whether it holds
/// wildcard handlers, so that selection can skip subtrees that cannot match an event. States
/// are in pre-order, so walking them backwards visits every child before its parent.
pub(super) fn index_subtree_events(states: &mut [StateNode], key_count: usize) {
    for node in states.iter_mut() {
        let mut keys = crate::engine::bits::StateBits::with_capacity(key_count);
        for (key, _) in &node.on_exact {
            keys.set(*key as usize, true);
        }
        node.subtree_events = keys;
        node.subtree_wildcards = !node.on_partial.is_empty() || !node.on_wildcard.is_empty();
    }
    for i in (1..states.len()).rev() {
        let Some(parent) = states[i].parent else { continue };
        let (keys, wildcards) = (states[i].subtree_events.clone(), states[i].subtree_wildcards);
        let parent = &mut states[parent.index()];
        parent.subtree_events.union_with(&keys);
        parent.subtree_wildcards |= wildcards;
    }
}
