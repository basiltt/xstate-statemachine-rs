//! The state sets of a microstep: exit and entry sets, transition domains, history targets
//! and conflict resolution -- XState v5's `stateUtils`, itself the SCXML Appendix D algorithm.

use crate::ids::{StateId, TransitionId};
use crate::machine::{Machine, NodeKind, Transition};

/// Set of states kept sorted in document order in an inline vector: configurations touched
/// by one microstep are small, so this never allocates in the common case.
#[derive(Default)]
pub(crate) struct StateSet {
    items: smallvec::SmallVec<[StateId; 16]>,
}

impl StateSet {
    #[inline]
    pub(crate) fn new(_machine: &Machine) -> Self {
        Self::default()
    }

    #[inline]
    pub(crate) fn insert(&mut self, s: StateId) {
        if let Err(pos) = self.items.binary_search(&s) {
            self.items.insert(pos, s);
        }
    }

    #[inline]
    pub(crate) fn contains(&self, s: StateId) -> bool {
        self.items.binary_search(&s).is_ok()
    }

    #[inline]
    pub(crate) fn is_disjoint(&self, other: &Self) -> bool {
        // Both sorted: merge-walk.
        let (mut i, mut j) = (0, 0);
        while i < self.items.len() && j < other.items.len() {
            match self.items[i].cmp(&other.items[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => return false,
            }
        }
        true
    }

    /// Members in document order.
    #[inline]
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = StateId> + '_ {
        self.items.iter().copied()
    }

    /// Members in document order, as a slice.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[StateId] {
        &self.items
    }

    /// Append `s`, which must sort after every member (members stay in document order).
    #[inline]
    fn push_last(&mut self, s: StateId) {
        debug_assert!(self.items.last().is_none_or(|&last| last < s));
        self.items.push(s);
    }
}

/// An entry set: borrowed from the machine's precomputed plans, or built for this microstep.
pub(crate) enum EntrySet<'m> {
    /// The fixed entry set of a single transition (see [`Plans`]).
    Planned(&'m [StateId]),
    /// Computed from the transitions and the recorded history.
    Built(StateSet),
}

impl EntrySet<'_> {
    /// Members in document order.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[StateId] {
        match self {
            Self::Planned(states) => states,
            Self::Built(set) => set.as_slice(),
        }
    }
}

/// Transitions enabled for one microstep; rarely more than a few.
pub(crate) type Enabled = smallvec::SmallVec<[TransitionId; 4]>;

/// XState's `removeConflictingTransitions`.
pub(crate) fn remove_conflicting<C>(
    machine: &Machine,
    enabled: &[TransitionId],
    state: &super::ActiveState<C>,
) -> Enabled {
    if enabled.len() <= 1 {
        // Nothing to conflict with.
        return Enabled::from_slice(enabled);
    }
    let mut filtered = Enabled::new();
    for &t1 in enabled {
        let mut preempted = false;
        let mut to_remove = Enabled::new();
        let exit1 = compute_exit_set(machine, &[t1], state);
        for &t2 in &filtered {
            let exit2 = compute_exit_set(machine, &[t2], state);
            if !exit1.is_disjoint(&exit2) {
                let s1 = machine.transition(t1).source;
                let s2 = machine.transition(t2).source;
                if machine.is_descendant(s1, s2) {
                    to_remove.push(t2);
                } else {
                    preempted = true;
                    break;
                }
            }
        }
        if !preempted {
            filtered.retain(|t| !to_remove.contains(t));
            filtered.push(t1);
        }
    }
    filtered
}

/// XState's `computeExitSet`.
pub(crate) fn compute_exit_set<C>(
    machine: &Machine,
    transitions: &[TransitionId],
    state: &super::ActiveState<C>,
) -> StateSet {
    let mut out = StateSet::new(machine);
    if let [tid] = transitions {
        exit_set_of_one(machine, *tid, state, &mut out);
        return out;
    }
    for &tid in transitions {
        let t = machine.transition(tid);
        if t.targets.is_empty() {
            continue;
        }
        let domain = match machine.plans() {
            Some(plans) => plans.domain(tid),
            None => transition_domain(machine, t, &state.history),
        };
        if t.reenter && Some(t.source) == domain {
            out.insert(t.source);
        }
        // The domain's proper descendants are the contiguous index range of its subtree.
        let (start, end) = match domain {
            Some(d) => (d.index() + 1, machine.subtree_end(d)),
            None => (0, usize::MAX),
        };
        for s in state.configuration.ones_in(start, end) {
            out.insert(StateId::from_index(s));
        }
    }
    out
}

/// [`compute_exit_set`] of a single transition: the members come out in document order (the
/// re-entered source precedes its subtree), so they are appended without searching.
fn exit_set_of_one<C>(
    machine: &Machine,
    tid: TransitionId,
    state: &super::ActiveState<C>,
    out: &mut StateSet,
) {
    let t = machine.transition(tid);
    if t.targets.is_empty() {
        return;
    }
    let domain = match machine.plans() {
        Some(plans) => plans.domain(tid),
        None => transition_domain(machine, t, &state.history),
    };
    if t.reenter && Some(t.source) == domain {
        out.push_last(t.source);
    }
    let (start, end) = match domain {
        Some(d) => (d.index() + 1, machine.subtree_end(d)),
        None => (0, usize::MAX),
    };
    for s in state.configuration.ones_in(start, end) {
        out.push_last(StateId::from_index(s));
    }
}

/// XState's `getTransitionDomain`. `None` means "above the root" (every active state exits).
pub(crate) fn transition_domain(
    machine: &Machine,
    t: &Transition,
    history: &super::HistoryMap,
) -> Option<StateId> {
    let targets = effective_targets(machine, t, history);
    if targets.is_empty() {
        return None;
    }
    if !t.reenter && targets.iter().all(|&x| machine.is_self_or_descendant(x, t.source)) {
        return Some(t.source);
    }
    // Least common *proper* ancestor of source and targets (symmetric, so the source can be
    // the walk's head).
    machine
        .ancestors(t.source, None)
        .find(|&anc| targets.iter().all(|&n| machine.is_descendant(n, anc)))
}

/// Ancestor chain of one state.
type Ancestors = smallvec::SmallVec<[StateId; 8]>;

/// Small list of target states.
type Targets = smallvec::SmallVec<[StateId; 2]>;

/// Targets with history pseudo-states replaced by the recorded (or default) states.
fn effective_targets(machine: &Machine, t: &Transition, history: &super::HistoryMap) -> Targets {
    let mut out = Targets::new();
    for &target in &t.targets {
        match machine.node(target).kind {
            NodeKind::History { .. } => out.extend(history_targets(machine, target, history)),
            _ => out.push(target),
        }
    }
    out
}

/// Recorded history for `h` (deep: the leaves, shallow: the direct children of its parent),
/// else its default target, else the parent's `initial`.
fn history_targets(machine: &Machine, h: StateId, history: &super::HistoryMap) -> Vec<StateId> {
    let node = machine.node(h);
    let deep = matches!(node.kind, NodeKind::History { deep: true, .. });
    if let Some(parent) = node.parent
        && let Some(recorded) = history.get(&parent)
    {
        let view: Vec<StateId> = recorded
            .iter()
            .copied()
            .filter(|&c| {
                if deep {
                    machine.node(c).is_leaf()
                } else {
                    machine.node(c).parent == Some(parent)
                }
            })
            .collect();
        if !view.is_empty() {
            return view;
        }
    }
    history_default(machine, h).into_iter().collect()
}

fn history_default(machine: &Machine, h: StateId) -> Option<StateId> {
    let node = machine.node(h);
    if let NodeKind::History { target: Some(t), .. } = node.kind {
        return Some(t);
    }
    let parent = node.parent?;
    match machine.node(parent).kind {
        NodeKind::Compound { initial } => Some(initial),
        _ => None,
    }
}

// ----------------------------------------------------------------------------- entry

/// XState's `computeEntrySet`. A single transition of a machine without history states has
/// a fixed entry set, computed once per machine (see [`Plans`]).
pub(crate) fn compute_entry_set<'m>(
    machine: &'m Machine,
    transitions: &[TransitionId],
    history: &super::HistoryMap,
) -> EntrySet<'m> {
    if let ([tid], Some(plans)) = (transitions, machine.plans()) {
        return EntrySet::Planned(plans.entry(*tid));
    }
    EntrySet::Built(compute_entry_set_dynamic(machine, transitions, history))
}

fn compute_entry_set_dynamic(
    machine: &Machine,
    transitions: &[TransitionId],
    history: &super::HistoryMap,
) -> StateSet {
    let mut to_enter = StateSet::new(machine);
    for &tid in transitions {
        let t = machine.transition(tid);
        let domain = transition_domain(machine, t, history);
        for &target in &t.targets {
            let node = machine.node(target);
            if !node.is_history() && (t.source != target || Some(t.source) != domain || t.reenter) {
                to_enter.insert(target);
            }
            add_descendants_to_enter(machine, target, history, &mut to_enter);
        }
        for s in effective_targets(machine, t, history) {
            let mut ancestors: Ancestors = machine.ancestors(s, domain).collect();
            if let Some(d) = domain.filter(|&d| machine.node(d).is_parallel()) {
                ancestors.push(d);
            }
            // XState: a re-entering transition on the root re-enters everything; otherwise the
            // domain itself is never re-entered, only its proper descendants.
            let reentrancy_domain =
                if t.source == StateId::ROOT && t.reenter { None } else { domain };
            add_ancestors_to_enter(machine, &ancestors, reentrancy_domain, history, &mut to_enter);
        }
    }
    to_enter
}

/// The entry set of the initial transition: the root and its default descendants. Nothing
/// is recorded in history before a machine starts, so this is a property of the machine.
pub(crate) fn initial_entry_set(machine: &Machine) -> smallvec::SmallVec<[StateId; 8]> {
    let mut to_enter = StateSet::new(machine);
    to_enter.insert(StateId::ROOT);
    add_descendants_to_enter(machine, StateId::ROOT, &super::HistoryMap::new(), &mut to_enter);
    smallvec::SmallVec::from_slice(to_enter.as_slice())
}

/// XState's `addDescendantStatesToEnter`.
pub(crate) fn add_descendants_to_enter(
    machine: &Machine,
    node: StateId,
    history: &super::HistoryMap,
    to_enter: &mut StateSet,
) {
    let n = machine.node(node);
    match n.kind {
        NodeKind::History { .. } => {
            let targets = history_targets(machine, node, history);
            for &s in &targets {
                to_enter.insert(s);
                add_descendants_to_enter(machine, s, history, to_enter);
            }
            for &s in &targets {
                let ancestors: Ancestors = machine.ancestors(s, n.parent).collect();
                add_ancestors_to_enter(machine, &ancestors, None, history, to_enter);
            }
        }
        NodeKind::Compound { initial } => {
            to_enter.insert(initial);
            add_descendants_to_enter(machine, initial, history, to_enter);
            let ancestors: Ancestors = machine.ancestors(initial, Some(node)).collect();
            add_ancestors_to_enter(machine, &ancestors, None, history, to_enter);
        }
        NodeKind::Parallel => {
            for &child in &n.children {
                if machine.node(child).is_history() {
                    continue;
                }
                if !to_enter.iter().any(|s| machine.is_descendant(s, child)) {
                    to_enter.insert(child);
                    add_descendants_to_enter(machine, child, history, to_enter);
                }
            }
        }
        NodeKind::Atomic | NodeKind::Final => {}
    }
}

/// XState's `addAncestorStatesToEnter`.
fn add_ancestors_to_enter(
    machine: &Machine,
    ancestors: &[StateId],
    reentrancy_domain: Option<StateId>,
    history: &super::HistoryMap,
    to_enter: &mut StateSet,
) {
    for &anc in ancestors {
        if reentrancy_domain.is_none_or(|d| machine.is_descendant(anc, d)) {
            to_enter.insert(anc);
        }
        if machine.node(anc).is_parallel() {
            for &child in &machine.node(anc).children {
                if machine.node(child).is_history() {
                    continue;
                }
                if !to_enter.iter().any(|s| machine.is_descendant(s, child)) {
                    to_enter.insert(child);
                    add_descendants_to_enter(machine, child, history, to_enter);
                }
            }
        }
    }
}

/// The states a single transition can exit, as an index range of the arena plus the source
/// when it re-enters itself: the domain's proper descendants (every state when the domain is
/// above the root).
#[inline]
pub(super) fn exit_range(
    machine: &Machine,
    t: &Transition,
    domain: Option<StateId>,
) -> (Option<StateId>, usize, usize) {
    let reentered = (t.reenter && Some(t.source) == domain).then_some(t.source);
    match domain {
        Some(d) => (reentered, d.index() + 1, machine.subtree_end(d)),
        None => (reentered, 0, machine.states().len()),
    }
}

/// Whether taking `t` alone does nothing but move configuration bits and run `t`'s actions.
fn is_plain(machine: &Machine, t: &Transition, domain: Option<StateId>, entry: &[StateId]) -> bool {
    if t.targets.is_empty() {
        // Targetless: exits and enters nothing.
        return true;
    }
    let quiet_exit = |s: StateId| {
        let n = machine.node(s);
        n.exit.is_empty() && n.after.is_empty() && n.invokes.is_empty()
    };
    let (reentered, start, end) = exit_range(machine, t, domain);
    reentered.is_none_or(quiet_exit)
        && (start..end).map(StateId::from_index).all(quiet_exit)
        && entry.iter().all(|&s| {
            let n = machine.node(s);
            n.entry.is_empty() && n.after.is_empty() && n.invokes.is_empty() && !n.is_final()
        })
}

/// Per-transition data that does not depend on the runtime state, for machines without
/// history states (where history cannot redirect a target): the transition domain, the
/// entry set of the transition taken alone, and whether taking it alone is *plain*.
#[derive(Debug)]
pub(crate) struct Plans {
    domains: smallvec::SmallVec<[Option<StateId>; 8]>,
    /// Transitions whose microstep, taken alone, is plain: no state it can exit has exit
    /// actions, `after` timers or invocations, and no state it enters has entry actions,
    /// `after` timers or invocations or is final. Such a microstep only swaps configuration
    /// bits around the transition's own actions (see `algorithm::plain_microstep`).
    plain: super::bits::StateBits,
    /// `entry[starts[t]..starts[t + 1]]` is transition `t`'s entry set, in document order.
    starts: smallvec::SmallVec<[u32; 9]>,
    entry: smallvec::SmallVec<[StateId; 16]>,
}

impl Plans {
    /// Plans for every transition, or `None` when the machine has history states.
    pub(crate) fn compute(machine: &Machine) -> Option<Self> {
        if machine.has_history() {
            return None;
        }
        let none = super::HistoryMap::new();
        let count = machine.transitions().len();
        let mut plans = Self {
            domains: smallvec::SmallVec::with_capacity(count),
            starts: smallvec::SmallVec::with_capacity(count + 1),
            entry: smallvec::SmallVec::new(),
            plain: super::bits::StateBits::with_capacity(count),
        };
        plans.starts.push(0);
        for (i, t) in machine.transitions().iter().enumerate() {
            let domain = transition_domain(machine, t, &none);
            plans.domains.push(domain);
            let tid = TransitionId::from_index(i);
            let entry = compute_entry_set_dynamic(machine, &[tid], &none);
            if is_plain(machine, t, domain, entry.as_slice()) {
                plans.plain.set(i, true);
            }
            plans.entry.extend(entry.iter());
            plans.starts.push(u32::try_from(plans.entry.len()).ok()?);
        }
        Some(plans)
    }

    #[inline]
    pub(super) fn domain(&self, t: TransitionId) -> Option<StateId> {
        self.domains[t.index()]
    }

    /// Whether `t`, taken alone, is a plain microstep (see [`Plans::plain`]).
    #[inline]
    pub(super) fn is_plain(&self, t: TransitionId) -> bool {
        self.plain.contains(t.index())
    }

    #[inline]
    pub(super) fn entry(&self, t: TransitionId) -> &[StateId] {
        let (from, to) = (self.starts[t.index()], self.starts[t.index() + 1]);
        &self.entry[from as usize..to as usize]
    }
}
