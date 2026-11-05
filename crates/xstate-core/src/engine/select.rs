//! Transition selection: event-driven (`transitionNode`) and eventless.

use super::Ctx;
use super::actions::evaluate_guard;
use super::sets::{Enabled, remove_conflicting};
use crate::context::Context;
use crate::error::Result;
use crate::event::Event;
use crate::ids::{StateId, TransitionId};
use crate::machine::{NodeKind, Transition, partial_matches};

/// XState's `transitionNode`: descend the active tree; a parallel node asks every region and
/// consults its own handlers only when no region had a match; a compound node consults its
/// own handlers only when its active child had none.
pub(super) fn select_transitions<C: Context>(
    cx: &mut Ctx<'_, C>,
    event: &Event,
) -> Result<Enabled> {
    let key = cx.machine.event_key(&event.ty);
    let mut enabled = Enabled::new();
    if cx.machine.has_parallel() {
        select_into(cx, StateId::ROOT, event, key, &mut enabled)?;
    } else if let Some(tid) = select_in_chain(cx, event, key)? {
        enabled.push(tid);
    }
    Ok(enabled)
}

/// [`select_transitions`] for a machine without parallel states: the configuration is one
/// chain from the root to a single leaf -- its last member in document order -- and the
/// descent of `transitionNode` amounts to asking the leaf, then each ancestor in turn.
fn select_in_chain<C: Context>(
    cx: &mut Ctx<'_, C>,
    event: &Event,
    key: Option<u32>,
) -> Result<Option<TransitionId>> {
    let root = cx.machine.node(StateId::ROOT);
    if !root.subtree_wildcards && !key.is_some_and(|k| root.subtree_events.contains(k as usize)) {
        // No state handles the event.
        return Ok(None);
    }
    let mut node = cx.state.configuration.last().map(StateId::from_index);
    while let Some(n) = node {
        if let Some(tid) = own_transition(cx, n, event, key)? {
            return Ok(Some(tid));
        }
        node = cx.machine.node(n).parent;
    }
    Ok(None)
}

/// Append what `node`'s active subtree selects (see [`select_transitions`]) to `out`.
fn select_into<C: Context>(
    cx: &mut Ctx<'_, C>,
    node: StateId,
    event: &Event,
    key: Option<u32>,
    out: &mut Enabled,
) -> Result<()> {
    let n = cx.machine.node(node);
    if !n.subtree_wildcards && !key.is_some_and(|k| n.subtree_events.contains(k as usize)) {
        // Nothing in this subtree handles the event.
        return Ok(());
    }
    let before = out.len();
    match n.kind {
        NodeKind::Parallel | NodeKind::Compound { .. } => {
            for &child in &n.children {
                if cx.state.is_active(child) {
                    select_into(cx, child, event, key, out)?;
                }
            }
        }
        NodeKind::Atomic | NodeKind::Final | NodeKind::History { .. } => {}
    }
    if out.len() == before
        && let Some(tid) = own_transition(cx, node, event, key)?
    {
        out.push(tid);
    }
    Ok(())
}

/// First candidate on `node` whose guard holds, in XState v5 order: the exact descriptor's
/// transitions, then matching partial wildcards (longest first), then `*`. Wildcard
/// candidates are tried even when an exact descriptor exists, so a failed exact guard falls
/// through to them.
fn own_transition<C: Context>(
    cx: &mut Ctx<'_, C>,
    node: StateId,
    event: &Event,
    key: Option<u32>,
) -> Result<Option<TransitionId>> {
    let machine = cx.machine;
    let n = machine.node(node);
    if let Some(k) = key
        && let Some((_, list)) = n.on_exact.iter().find(|(ek, _)| *ek == k)
        && let Some(tid) = first_enabled(cx, list, event)?
    {
        return Ok(Some(tid));
    }
    for (descriptor, list) in &n.on_partial {
        if partial_matches(descriptor, &event.ty)
            && let Some(tid) = first_enabled(cx, list, event)?
        {
            return Ok(Some(tid));
        }
    }
    first_enabled(cx, &n.on_wildcard, event)
}

/// First transition of `list` whose guard holds.
#[inline]
fn first_enabled<C: Context>(
    cx: &mut Ctx<'_, C>,
    list: &[TransitionId],
    event: &Event,
) -> Result<Option<TransitionId>> {
    for &tid in list {
        if guard_holds(cx, cx.machine.transition(tid), event)? {
            return Ok(Some(tid));
        }
    }
    Ok(None)
}

pub(super) fn guard_holds<C: Context>(
    cx: &mut Ctx<'_, C>,
    t: &Transition,
    event: &Event,
) -> Result<bool> {
    match &t.guard {
        None => Ok(true),
        Some(g) => evaluate_guard(cx, g, event),
    }
}

/// XState's `selectEventlessTransitions`: per atomic state, the first `always` transition up
/// the ancestor chain whose guard holds; then conflict resolution. The atomic states are
/// visited in the order XState's snapshot holds them -- entry order, see
/// `ActiveState::entered_at` -- so the leaf entered first is asked first and wins a conflict.
pub(super) fn select_eventless_transitions<C: Context>(
    cx: &mut Ctx<'_, C>,
    event: &Event,
) -> Result<Enabled> {
    let machine = cx.machine;
    let mut leaves = cx.state.leaves(machine);
    if leaves.len() > 1 {
        let entered = &cx.state.entered_at;
        leaves.sort_unstable_by_key(|s| (entered.get(s.index()).copied().unwrap_or(0), *s));
    }
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
