//! The macrostep loop, microsteps, and state exit and entry -- a direct transcription of
//! XState v5's `stateUtils` (itself the SCXML Appendix D algorithm). Selection lives in
//! [`super::select`], the exit / entry set computation in [`super::sets`].

use serde_json::Value;

use super::actions::run_actions;
use super::select::{select_eventless_transitions, select_transitions};
use super::sets::{
    Enabled, StateSet, add_descendants_to_enter, compute_entry_set, compute_exit_set,
    remove_conflicting,
};
use super::{Ctx, Effect, Status};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::ids::{StateId, TransitionId};
use crate::machine::{Machine, NodeKind};

// ----------------------------------------------------------------------------- entry points

pub(super) fn start<C: Context>(cx: &mut Ctx<'_, C>) -> Result<()> {
    let machine = cx.machine;
    let init = Event::init();
    let mut to_enter = StateSet::new(machine);
    to_enter.insert(StateId::ROOT);
    add_descendants_to_enter(machine, StateId::ROOT, &cx.state.history, &mut to_enter);
    enter_states(cx, &to_enter, &init, false)?;
    exit_all_when_done(cx, &init)?;
    cx.report.microsteps += 1;
    settle(cx, &init)
}

/// XState runs the exit actions of every remaining active state (reverse document order) as
/// soon as the machine is done -- in `microstep`, which also covers the initial transition.
fn exit_all_when_done<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<()> {
    if cx.state.status == Status::Done {
        let remaining: smallvec::SmallVec<[StateId; 16]> = cx.state.configuration().collect();
        for &s in remaining.iter().rev() {
            run_actions(cx, &cx.machine.node(s).exit, event)?;
        }
    }
    Ok(())
}

pub(super) fn macrostep<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<()> {
    let enabled = select_transitions(cx, event)?;
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
        microstep(cx, &enabled, event, false)?;
    }
    if !cx.machine.has_always() && cx.state.internal_queue.is_empty() {
        // Nothing eventless to select and nothing raised: `settle` would stop at once.
        return Ok(());
    }
    settle(cx, event)
}

/// Run eventless transitions and the internal queue until the configuration is stable
/// (XState v5 `macrostep` loop). After an eventless microstep that changed nothing (XState: the
/// microstep returned the same snapshot -- see `ActiveState::mutations`), eventless transitions
/// are not re-selected until an internal event has been processed, so a self-targeting
/// `always` runs once instead of looping.
fn settle<C: Context>(cx: &mut Ctx<'_, C>, event: &Event) -> Result<()> {
    let limit = cx.machine.policies().max_iterations;
    // The event being settled: the delivered one, then each internal event in turn.
    let mut raised: Option<Event> = None;
    let mut iterations: u32 = 0;
    let mut select_eventless = true;
    while cx.state.status == Status::Running {
        iterations += 1;
        if iterations > limit {
            let err = Error::RunawayChain {
                machine: cx.machine.id().to_owned(),
                limit,
                event: raised.as_ref().unwrap_or(event).ty.to_string(),
            };
            cx.state.status = Status::Error;
            cx.state.error = Some(err.to_string());
            cx.state.internal_queue.clear();
            return Err(err);
        }
        let mut enabled = if select_eventless && cx.machine.has_always() {
            select_eventless_transitions(cx, raised.as_ref().unwrap_or(event))?
        } else {
            Enabled::new()
        };
        let was_eventless = !enabled.is_empty();
        if !was_eventless {
            let Some(next) = cx.state.internal_queue.pop_front() else { break };
            enabled = select_transitions(cx, raised.insert(next))?;
        }
        let before = cx.state.mutations;
        if !enabled.is_empty() {
            microstep(cx, &enabled, raised.as_ref().unwrap_or(event), was_eventless)?;
        }
        select_eventless = !was_eventless || cx.state.mutations != before;
    }
    Ok(())
}

// ----------------------------------------------------------------------------- microstep

/// One SCXML microstep. `eventless` marks a microstep of the eventless pass, whose "did it
/// change anything" answer decides whether the macrostep selects eventless transitions again.
fn microstep<C: Context>(
    cx: &mut Ctx<'_, C>,
    enabled: &[TransitionId],
    event: &Event,
    eventless: bool,
) -> Result<()> {
    let machine = cx.machine;
    let filtered = remove_conflicting(machine, enabled, cx.state);
    cx.report.microsteps += 1;
    if let [only] = filtered.as_slice() {
        cx.report.transitions.push(*only);
    } else {
        cx.report.transitions.extend(filtered.iter().copied());
    }

    let to_exit = compute_exit_set(machine, &filtered, cx.state);
    let records_history = machine.has_history()
        && to_exit.iter().any(|s| !machine.node(s).history_children.is_empty());
    // An eventless microstep must know up front whether it re-enters exactly what it exits (to
    // watch the context). Without a history record the exit does not change what the entry
    // set depends on, so it can be computed first; other microsteps compute it after the
    // actions as usual.
    let early_entry = (eventless && !records_history)
        .then(|| compute_entry_set(machine, &filtered, &cx.state.history));
    let context_before = match &early_entry {
        Some(entry) if entry.same_members(&to_exit) => context_image(cx),
        _ => None,
    };

    exit_states(cx, &to_exit, event)?;
    for &tid in &filtered {
        run_actions(cx, &machine.transition(tid).actions, event)?;
    }
    let to_enter =
        early_entry.unwrap_or_else(|| compute_entry_set(machine, &filtered, &cx.state.history));
    // XState keeps the previous snapshot -- and the order of its states -- when the microstep
    // re-enters exactly what it exited and records no history.
    let preserved = !records_history && to_enter.same_members(&to_exit);
    enter_states(cx, &to_enter, event, preserved)?;
    note_snapshot_change(cx, preserved, &to_exit, &to_enter, context_before);
    exit_all_when_done(cx, event)
}

/// The context as JSON, to tell whether user actions change it in place (the analogue of an
/// XState `assign`). A context that does not serialise cannot be compared: it counts as
/// changed right away (`None`).
fn context_image<C: Context>(cx: &mut Ctx<'_, C>) -> Option<Value> {
    let image = serde_json::to_value(&cx.state.context).ok();
    if image.is_none() {
        cx.state.mutations += 1;
    }
    image
}

/// Bump `ActiveState::mutations` when the microstep produced a new snapshot in XState's
/// sense: the configuration changed, or -- with the configuration preserved -- an actor was
/// started or stopped, or user actions changed the context (`context_before` is only taken
/// for an eventless microstep; an `assign` and a history record bump on their own).
fn note_snapshot_change<C: Context>(
    cx: &mut Ctx<'_, C>,
    preserved: bool,
    to_exit: &StateSet,
    to_enter: &StateSet,
    context_before: Option<Value>,
) {
    let machine = cx.machine;
    let changed = !preserved
        || to_exit.iter().chain(to_enter.iter()).any(|s| !machine.node(s).invokes.is_empty())
        || context_before.is_some_and(|before| {
            serde_json::to_value(&cx.state.context).ok().as_ref() != Some(&before)
        });
    if changed {
        cx.state.mutations += 1;
    }
}

// ----------------------------------------------------------------------------- exit

fn exit_states<C: Context>(cx: &mut Ctx<'_, C>, to_exit: &StateSet, event: &Event) -> Result<()> {
    let machine = cx.machine;
    if to_exit.is_empty() {
        return Ok(());
    }
    // Record history before anything leaves the configuration. The record is keyed by the
    // exited state and holds every active descendant (the Python snapshot layout); the
    // shallow/deep view a history node needs is derived from it on restore.
    for s in to_exit.iter().filter(|_| machine.has_history()) {
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
        cx.report.changed = true;
    }
    Ok(())
}

/// XState's `enterStates`. With `keep_order` (the microstep re-enters exactly what it exited
/// and records no history) the states keep their entry stamps: XState keeps the previous
/// snapshot, so their position in its state list does not change.
fn enter_states<C: Context>(
    cx: &mut Ctx<'_, C>,
    to_enter: &StateSet,
    event: &Event,
    keep_order: bool,
) -> Result<()> {
    let machine = cx.machine;
    let mut completed = StateSet::new(machine);
    for s in to_enter.iter() {
        if cx.state.status != Status::Running {
            break;
        }
        let node = machine.node(s);
        cx.state.configuration.set(s.index(), true);
        if s != StateId::ROOT {
            cx.state.entry_seq += 1;
        }
        if !keep_order && let Some(stamp) = cx.state.entered_at.get_mut(s.index()) {
            *stamp = cx.state.entry_seq;
        }
        cx.report.changed = true;

        run_actions(cx, &node.entry, event)?;
        arm_entry_effects(cx, s, event)?;
        if node.is_final() {
            final_state_entered(cx, s, &mut completed);
        }
    }
    Ok(())
}

/// The `after` timers and invoked services of a state just entered.
fn arm_entry_effects<C: Context>(cx: &mut Ctx<'_, C>, s: StateId, event: &Event) -> Result<()> {
    let node = cx.machine.node(s);
    for (index, after) in node.after.iter().enumerate() {
        let Some(delay_ms) = super::usable_delay(cx, &after.delay, event)? else { continue };
        // `xstate.after.<key>.<state id>`, interned at compile time.
        let ev = match node.after_events.get(index) {
            Some(ty) => Event::engine(ty.clone(), Value::Null, EventOrigin::After),
            None => Event::after(&after.key, &node.id),
        };
        cx.effects.push(Effect::ArmTimer {
            state: s,
            index,
            entry_seq: cx.state.entry_seq,
            delay_ms,
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
    Ok(())
}

/// XState's final-state handling in `enterStates`: `done.state` for the compound parent, then
/// for every parallel ancestor (from the parent if it is parallel, else the grandparent) whose
/// regions are all final; past the root the machine is done. `completed` keeps a parallel
/// state from being reported twice in one entry pass.
fn final_state_entered<C: Context>(cx: &mut Ctx<'_, C>, s: StateId, completed: &mut StateSet) {
    let machine = cx.machine;
    let node = machine.node(s);
    let Some(parent) = node.parent else { return };
    let parent_node = machine.node(parent);
    let mut marker = if parent_node.is_parallel() { Some(parent) } else { parent_node.parent };
    let mut root_completion = marker.unwrap_or(s);
    let mut done_parallel: smallvec::SmallVec<[StateId; 4]> = smallvec::SmallVec::new();
    while let Some(anc) = marker {
        if !machine.node(anc).is_parallel()
            || completed.contains(anc)
            || !in_final_state(machine, cx.state, anc)
        {
            break;
        }
        completed.insert(anc);
        done_parallel.push(anc);
        root_completion = anc;
        marker = machine.node(anc).parent;
    }
    if marker.is_none() {
        // The machine is done: `finish` drops the internal queue, so the `done.state` events
        // this entry would raise are never seen and are not built.
        finish(cx, root_completion);
        return;
    }
    if parent_node.is_compound() {
        let output = node.output.clone().unwrap_or(Value::Null);
        cx.state.internal_queue.push_back(Event::done_state(&parent_node.id, output));
    }
    for anc in done_parallel {
        cx.state.internal_queue.push_back(Event::done_state(&machine.node(anc).id, Value::Null));
    }
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
        // A history pseudo-state is not a region (XState's `getChildren`).
        NodeKind::Parallel => n
            .children
            .iter()
            .filter(|&&c| !machine.node(c).is_history())
            .all(|&c| in_final_state(machine, state, c)),
        NodeKind::Final => true,
        _ => false,
    }
}
