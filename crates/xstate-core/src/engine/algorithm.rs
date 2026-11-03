//! Selection, conflict resolution, exit/entry computation and the macrostep loop — a direct
//! transcription of XState v5's `stateUtils` (itself the SCXML Appendix D algorithm).

use serde_json::Value;

use super::actions::{evaluate_guard, run_actions};
use super::{Ctx, Effect, Status};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::ids::{StateId, TransitionId};
use crate::machine::{Machine, NodeKind, Transition};

/// Set of states kept sorted in document order in an inline vector: configurations touched
/// by one microstep are small, so this never allocates in the common case.
#[derive(Default)]
struct StateSet {
    items: smallvec::SmallVec<[StateId; 16]>,
}

impl StateSet {
    #[inline]
    fn new(_machine: &Machine) -> Self {
        Self::default()
    }

    #[inline]
    fn insert(&mut self, s: StateId) {
        if let Err(pos) = self.items.binary_search(&s) {
            self.items.insert(pos, s);
        }
    }

    #[inline]
    fn contains(&self, s: StateId) -> bool {
        self.items.binary_search(&s).is_ok()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[inline]
    fn is_disjoint(&self, other: &Self) -> bool {
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
    fn iter(&self) -> impl DoubleEndedIterator<Item = StateId> + '_ {
        self.items.iter().copied()
    }
}

/// Transitions enabled for one microstep; rarely more than a few.
type Enabled = smallvec::SmallVec<[TransitionId; 4]>;

// ----------------------------------------------------------------------------- entry points

pub(super) fn start<C: Context>(cx: &mut Ctx<'_, C>) -> Result<()> {
    let machine = cx.machine;
    let init = Event::init();
    let mut to_enter = StateSet::new(machine);
    to_enter.insert(StateId::ROOT);
    add_descendants_to_enter(machine, StateId::ROOT, &cx.state.history, &mut to_enter);
    enter_states(cx, &to_enter, &init)?;
    exit_all_when_done(cx, &init)?;
    cx.report.microsteps += 1;
    settle(cx, init)
}

/// XState runs the exit actions of every remaining active state (reverse document order) as
/// soon as the machine is done -- in `microstep`, which also covers the initial transition.
fn exit_all_when_done<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<()> {
    if cx.state.status == Status::Done {
        let remaining: Vec<StateId> = cx.state.configuration().collect();
        for &s in remaining.iter().rev() {
            run_actions(cx, &cx.machine.node(s).exit, event)?;
        }
    }
    Ok(())
}

pub(super) fn macrostep<C: Context>(cx: &mut Ctx<'_, C>, event: Event) -> Result<()> {
    let enabled = select_transitions(cx, &event)?;
    if enabled.is_empty() {
        cx.report.unhandled = true;
        if event.origin == EventOrigin::ErrorActor {
            // An unhandled service failure fails the machine (XState v5 macrostep).
            let message = match event.payload.get("error") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "service failed".to_owned(),
            };
            cx.state.status = Status::Error;
            cx.state.error = Some(message);
            cx.state.internal_queue.clear();
            return Ok(());
        }
    } else {
        microstep(cx, &enabled, &event)?;
    }
    settle(cx, event)
}

/// Run eventless transitions and the internal queue until the configuration is stable
/// (XState v5 `macrostep` loop). After an eventless microstep that changed nothing, eventless
/// transitions are not re-selected until an internal event has been processed.
fn settle<C: Context>(cx: &mut Ctx<'_, C>, mut current: Event) -> Result<()> {
    let limit = cx.machine.policies().max_iterations;
    let mut iterations: u32 = 0;
    let mut select_eventless = true;
    while cx.state.status == Status::Running {
        iterations += 1;
        if iterations > limit {
            let err = Error::RunawayChain {
                machine: cx.machine.id().to_owned(),
                limit,
                event: current.ty.to_string(),
            };
            cx.state.status = Status::Error;
            cx.state.error = Some(err.to_string());
            cx.state.internal_queue.clear();
            return Err(err);
        }
        let mut enabled = if select_eventless && cx.machine.has_always() {
            select_eventless_transitions(cx, &current)?
        } else {
            Enabled::new()
        };
        let was_eventless = !enabled.is_empty();
        if !was_eventless {
            let Some(next) = cx.state.internal_queue.pop_front() else { break };
            current = next;
            enabled = select_transitions(cx, &current)?;
        }
        let before = cx.state.mutations;
        if !enabled.is_empty() {
            microstep(cx, &enabled, &current)?;
        }
        select_eventless = !was_eventless || cx.state.mutations != before;
    }
    Ok(())
}

// ----------------------------------------------------------------------------- selection

/// XState's `transitionNode`: descend the active tree; a parallel node asks every region and
/// consults its own handlers only when no region had a match; a compound node consults its
/// own handlers only when its active child had none.
fn select_transitions<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<Enabled> {
    fn select<C: Context>(
        cx: &mut Ctx<'_, C>,
        node: StateId,
        event: &Event,
        key: Option<u32>,
    ) -> Result<Enabled> {
        let machine = cx.machine;
        let n = machine.node(node);
        let mut inner = Enabled::new();
        match n.kind {
            NodeKind::Parallel | NodeKind::Compound { .. } => {
                for &child in &n.children {
                    if cx.state.is_active(child) {
                        inner.extend(select(cx, child, event, key)?);
                    }
                }
            }
            NodeKind::Atomic | NodeKind::Final | NodeKind::History { .. } => {}
        }
        if !inner.is_empty() {
            return Ok(inner);
        }
        Ok(own_transition(cx, node, event, key)?.into_iter().collect())
    }
    let key = cx.machine.event_key(&event.ty);
    select(cx, StateId::ROOT, event, key)
}

/// First candidate on `node` whose guard holds.
fn own_transition<C: Context>(
    cx: &mut Ctx<'_, C>,
    node: StateId,
    event: &Event,
    key: Option<u32>,
) -> Result<Option<TransitionId>> {
    let machine = cx.machine;
    for tid in machine.node(node).candidates(key, &event.ty) {
        if guard_holds(cx, machine.transition(tid), event)? {
            return Ok(Some(tid));
        }
    }
    Ok(None)
}

fn guard_holds<C: Context>(cx: &mut Ctx<'_, C>, t: &Transition, event: &Event) -> Result<bool> {
    match &t.guard {
        None => Ok(true),
        Some(g) => evaluate_guard(cx, g, event),
    }
}

/// XState's `selectEventlessTransitions`: per atomic state, the first `always` transition up
/// the ancestor chain whose guard holds; then conflict resolution.
fn select_eventless_transitions<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<Enabled> {
    let machine = cx.machine;
    let leaves = cx.state.leaves(machine);
    let mut enabled = Enabled::new();
    for leaf in leaves {
        let chain = std::iter::once(leaf).chain(machine.ancestors(leaf, None));
        'walk: for node in chain {
            let always = &machine.node(node).always;
            if always.is_empty() {
                continue;
            }
            for &tid in always {
                if guard_holds(cx, machine.transition(tid), event)? {
                    if !enabled.contains(&tid) {
                        enabled.push(tid);
                    }
                    break 'walk;
                }
            }
        }
    }
    Ok(remove_conflicting(machine, &enabled, cx.state))
}

// ----------------------------------------------------------------------------- microstep

fn microstep<C: Context>(
    cx: &mut Ctx<'_, C>,
    enabled: &[TransitionId],
    event: &Event,
) -> Result<()> {
    let machine = cx.machine;
    let filtered = remove_conflicting(machine, enabled, cx.state);
    cx.report.microsteps += 1;
    cx.report.transitions.extend(filtered.iter().copied());

    exit_states(cx, &filtered, event)?;

    for &tid in &filtered {
        run_actions(cx, &machine.transition(tid).actions, event)?;
    }

    let to_enter = compute_entry_set(machine, &filtered, &cx.state.history);
    enter_states(cx, &to_enter, event)?;
    exit_all_when_done(cx, event)
}

/// XState's `removeConflictingTransitions`.
fn remove_conflicting<C>(
    machine: &Machine,
    enabled: &[TransitionId],
    state: &super::ActiveState<C>,
) -> Enabled {
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

// ----------------------------------------------------------------------------- exit

fn exit_states<C: Context>(
    cx: &mut Ctx<'_, C>,
    transitions: &[TransitionId],
    event: &Event,
) -> Result<()> {
    let machine = cx.machine;
    let to_exit = compute_exit_set(machine, transitions, cx.state);
    if to_exit.is_empty() {
        return Ok(());
    }
    // Record history before anything leaves the configuration. The record is keyed by the
    // exited state and holds every active descendant (the Python snapshot layout); the
    // shallow/deep view a history node needs is derived from it on restore.
    for s in to_exit.iter() {
        if machine.node(s).history_children.is_empty() {
            continue;
        }
        let recorded: Vec<StateId> =
            cx.state.configuration().filter(|&c| machine.is_descendant(c, s)).collect();
        cx.state.history.insert(s, recorded);
        cx.state.mutations += 1;
    }
    // Reverse document order.
    for s in to_exit.iter().rev() {
        let node = machine.node(s);
        cx.effects.push(Effect::CancelTimers(s));
        run_actions(cx, &node.exit, event)?;
        for inv in &node.invokes {
            cx.effects.push(Effect::StopActor(inv.id.clone()));
        }
        cx.state.configuration.set(s.index(), false);
        cx.state.mutations += 1;
        cx.report.changed = true;
    }
    Ok(())
}

/// XState's `computeExitSet`.
fn compute_exit_set<C>(
    machine: &Machine,
    transitions: &[TransitionId],
    state: &super::ActiveState<C>,
) -> StateSet {
    let mut out = StateSet::new(machine);
    for &tid in transitions {
        let t = machine.transition(tid);
        if t.targets.is_empty() {
            continue;
        }
        let domain = transition_domain(machine, t, &state.history);
        if t.reenter && Some(t.source) == domain {
            out.insert(t.source);
        }
        for s in state.configuration() {
            if domain.is_none_or(|d| machine.is_descendant(s, d)) {
                out.insert(s);
            }
        }
    }
    out
}

/// XState's `getTransitionDomain`. `None` means "above the root" (every active state exits).
fn transition_domain(
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

/// XState's `computeEntrySet`.
fn compute_entry_set(
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

/// XState's `addDescendantStatesToEnter`.
fn add_descendants_to_enter(
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

fn enter_states<C: Context>(cx: &mut Ctx<'_, C>, to_enter: &StateSet, event: &Event) -> Result<()> {
    let machine = cx.machine;
    let mut completed = StateSet::new(machine);
    for s in to_enter.iter() {
        if cx.state.status != Status::Running {
            break;
        }
        let node = machine.node(s);
        cx.state.configuration.set(s.index(), true);
        cx.state.mutations += 1;
        if s != StateId::ROOT {
            cx.state.entry_seq += 1;
        }
        cx.report.changed = true;

        run_actions(cx, &node.entry, event)?;
        for (index, after) in node.after.iter().enumerate() {
            let delay_ms = super::resolve_delay(cx.logic, &after.delay, &cx.state.context, event)?;
            if !delay_ms.is_finite() {
                continue;
            }
            let ev = Event::after(&after.key, &node.id);
            cx.effects.push(Effect::ArmTimer {
                state: s,
                index,
                entry_seq: cx.state.entry_seq,
                delay_ms: delay_ms.max(0.0),
                event: ev,
            });
        }
        for inv in &node.invokes {
            cx.effects.push(Effect::StartInvoke {
                state: s,
                invoke: inv.clone(),
                input: inv.input.clone(),
            });
        }

        if node.is_final() {
            let Some(parent) = node.parent else { continue };
            let parent_node = machine.node(parent);
            let mut marker =
                if parent_node.is_parallel() { Some(parent) } else { parent_node.parent };
            let mut root_completion = marker.unwrap_or(s);
            if parent_node.is_compound() {
                let output = node.output.clone().unwrap_or(Value::Null);
                cx.state.internal_queue.push_back(Event::done_state(&parent_node.id, output));
            }
            while let Some(anc) = marker {
                if !machine.node(anc).is_parallel()
                    || completed.contains(anc)
                    || !in_final_state(machine, cx.state, anc)
                {
                    break;
                }
                completed.insert(anc);
                cx.state
                    .internal_queue
                    .push_back(Event::done_state(&machine.node(anc).id, Value::Null));
                root_completion = anc;
                marker = machine.node(anc).parent;
            }
            if marker.is_none() {
                finish(cx, root_completion);
            }
        }
    }
    Ok(())
}

fn finish<C: Context>(cx: &mut Ctx<'_, C>, _completion_node: StateId) {
    cx.state.status = Status::Done;
    cx.state.output = cx.machine.output().cloned().unwrap_or(Value::Null);
    cx.state.internal_queue.clear();
}

/// XState's `isInFinalState`.
fn in_final_state<C>(machine: &Machine, state: &super::ActiveState<C>, node: StateId) -> bool {
    let n = machine.node(node);
    match n.kind {
        NodeKind::Compound { .. } => {
            n.children.iter().any(|&c| machine.node(c).is_final() && state.is_active(c))
        }
        NodeKind::Parallel => n.children.iter().all(|&c| in_final_state(machine, state, c)),
        NodeKind::Final => true,
        _ => false,
    }
}
