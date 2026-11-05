//! Action execution and guard evaluation.

use serde_json::Value;

use super::{Ctx, Effect};
use crate::config::{ActionErrorPolicy, GuardErrorPolicy};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::logic::ActionScope;
use crate::machine::{ActionDef, Builtin, Guard, SendTarget};

/// Execute an action list in order. Under `actionErrorPolicy: "continue"` a failing action
/// aborts the rest of *this* list and the step goes on; otherwise the error propagates as
/// [`Error::TransitionFailed`] for the caller to roll back or fail. A failure inside a nested
/// list (`choose`) counts as a failure of the action that holds it, so the policy applies to
/// it exactly as to a top-level one.
pub(super) fn run_actions<C: Context>(
    cx: &mut Ctx<'_, C>,
    actions: &[ActionDef],
    event: &Event,
) -> Result<()> {
    if actions.is_empty() {
        return Ok(());
    }
    let Err(err) = run_list(cx, actions, event) else { return Ok(()) };
    match cx.machine.policies().action_error_policy {
        ActionErrorPolicy::Continue => {
            tracing::warn!(target: "xstate", error = %err, "action failed; continuing");
            Ok(())
        }
        ActionErrorPolicy::Rollback | ActionErrorPolicy::Fail => Err(err),
    }
}

/// Run a list, stopping at the first failure, which comes back as
/// [`Error::TransitionFailed`] naming the innermost failing action.
fn run_list<C: Context>(cx: &mut Ctx<'_, C>, actions: &[ActionDef], event: &Event) -> Result<()> {
    for action in actions {
        match run_action(cx, action, event) {
            Ok(()) => {}
            Err(err @ Error::TransitionFailed { .. }) => return Err(err),
            Err(err) => {
                return Err(Error::TransitionFailed {
                    action: action_name(action),
                    source: Box::new(err),
                });
            }
        }
    }
    Ok(())
}

fn action_name(action: &ActionDef) -> String {
    match action {
        ActionDef::User { name, .. } => name.to_string(),
        ActionDef::Builtin { builtin, .. } => format!("xstate.{}", builtin_name(builtin)),
    }
}

fn builtin_name(b: &Builtin) -> &'static str {
    match b {
        Builtin::Assign(_) => "assign",
        Builtin::Raise { .. } => "raise",
        Builtin::SendTo { .. } => "sendTo",
        Builtin::ForwardTo(_) => "forwardTo",
        Builtin::Escalate(_) => "escalate",
        Builtin::Log { .. } => "log",
        Builtin::Cancel(_) => "cancel",
        Builtin::StopChild(_) => "stopChild",
        Builtin::SpawnChild { .. } => "spawnChild",
        Builtin::Emit(_) => "emit",
        Builtin::Choose(_) => "choose",
    }
}

fn run_action<C: Context>(cx: &mut Ctx<'_, C>, action: &ActionDef, event: &Event) -> Result<()> {
    match action {
        ActionDef::User { name, params } => run_user(cx, name, params, event),
        // A user action spelled like a built-in wins over it (XState: a string action type
        // names an implementation).
        ActionDef::Builtin { name, params, .. } if cx.logic.builtin_override(name).is_some() => {
            run_user(cx, name, params, event)
        }
        ActionDef::Builtin { builtin, .. } => run_builtin(cx, builtin, event),
    }
}

fn run_user<C: Context>(
    cx: &mut Ctx<'_, C>,
    name: &std::sync::Arc<str>,
    params: &Value,
    event: &Event,
) -> Result<()> {
    if cx.pure {
        cx.report.planned.push(crate::pure::PlannedAction {
            name: std::sync::Arc::clone(name),
            params: params.clone(),
        });
        cx.report.actions.push(std::sync::Arc::clone(name));
        return Ok(());
    }
    let logic = cx.logic;
    let f = logic.get_action(name)?;
    let mut local: Vec<Effect> = Vec::new();
    {
        let mut scope =
            ActionScope { context: &mut cx.state.context, event, params, effects: &mut local };
        f(&mut scope)?;
    }
    cx.report.actions.push(std::sync::Arc::clone(name));
    cx.report.changed = true;
    for effect in local {
        match effect {
            Effect::Raise(ev) => cx.state.internal_queue.push_back(ev),
            other => cx.effects.push(other),
        }
    }
    Ok(())
}

fn run_builtin<C: Context>(cx: &mut Ctx<'_, C>, b: &Builtin, event: &Event) -> Result<()> {
    match b {
        Builtin::Assign(patch) => {
            cx.state.context.assign_json(patch)?;
            cx.state.mutations += 1;
            cx.report.changed = true;
        }
        Builtin::Raise { event: ev, delay, id } => {
            let raised = event_from_value(ev, EventOrigin::Internal)?;
            match delay {
                None => cx.state.internal_queue.push_back(raised),
                Some(d) => {
                    // An unusable delay disables this one send (logged), like an `after` timer.
                    let Some(delay_ms) = super::usable_delay(cx, d, event)? else {
                        return Ok(());
                    };
                    cx.effects.push(Effect::DelayedRaise {
                        event: raised,
                        delay_ms,
                        id: id.clone(),
                    });
                }
            }
        }
        Builtin::SendTo { to, event: ev, delay, id } => {
            let sent = event_from_value(ev, EventOrigin::External)?;
            let delay_ms = match delay {
                None => None,
                Some(d) => match super::usable_delay(cx, d, event)? {
                    Some(ms) => Some(ms),
                    None => return Ok(()),
                },
            };
            if *to == SendTarget::SelfActor && delay_ms.is_none() {
                cx.state.internal_queue.push_back(Event { origin: EventOrigin::Internal, ..sent });
            } else {
                cx.effects.push(Effect::SendTo {
                    to: to.clone(),
                    event: sent,
                    delay_ms,
                    id: id.clone(),
                });
            }
        }
        Builtin::ForwardTo(to) => {
            cx.effects.push(Effect::SendTo {
                to: to.clone(),
                event: event.clone(),
                delay_ms: None,
                id: None,
            });
        }
        Builtin::Escalate(error) => cx.effects.push(Effect::Escalate(error.clone())),
        Builtin::Log { expr, label } => {
            cx.effects.push(Effect::Log { expr: expr.clone(), label: label.clone() });
        }
        Builtin::Cancel(id) => cx.effects.push(Effect::CancelSend(id.clone())),
        // Starting or stopping a child changes XState's snapshot (`children`).
        Builtin::StopChild(id) => {
            cx.effects.push(Effect::StopActor(id.clone()));
            cx.state.mutations += 1;
        }
        Builtin::SpawnChild { src, id, system_id, input } => {
            cx.effects.push(Effect::Spawn {
                src: src.clone(),
                id: id.clone().unwrap_or_else(|| src.clone()),
                system_id: system_id.clone(),
                input: input.clone(),
            });
            cx.state.mutations += 1;
        }
        Builtin::Emit(ev) => cx.effects.push(Effect::Emit(ev.clone())),
        Builtin::Choose(branches) => {
            for branch in branches {
                let taken = match &branch.guard {
                    None => true,
                    Some(g) => evaluate_guard(cx, g, event)?,
                };
                if taken {
                    return run_list(cx, &branch.actions, event);
                }
            }
        }
    }
    Ok(())
}

/// Build an event from a `raise`/`sendTo` param: a bare type string or an event object.
fn event_from_value(value: &Value, origin: EventOrigin) -> Result<Event> {
    let mut ev = Event::from_json(value.clone())?;
    ev.origin = origin;
    Ok(ev)
}

/// Evaluate a guard expression under the machine's `guardErrorPolicy`.
pub(super) fn evaluate_guard<C: Context>(
    cx: &mut Ctx<'_, C>,
    guard: &Guard,
    event: &Event,
) -> Result<bool> {
    match guard {
        Guard::Named { name, params } => {
            let logic = cx.logic;
            let f = logic.get_guard(name)?;
            match f(&cx.state.context, event, params) {
                Ok(v) => Ok(v),
                Err(err) => match cx.machine.policies().guard_error_policy {
                    GuardErrorPolicy::False => {
                        tracing::warn!(target: "xstate", guard = %name, error = %err, "guard failed; treated as false");
                        Ok(false)
                    }
                    GuardErrorPolicy::True => {
                        tracing::warn!(target: "xstate", guard = %name, error = %err, "guard failed; treated as true");
                        Ok(true)
                    }
                    GuardErrorPolicy::Raise => Err(err),
                },
            }
        }
        Guard::StateIn { state, .. } => Ok(cx.state.is_active(*state)),
        Guard::Not(inner) => Ok(!evaluate_guard(cx, inner, event)?),
        Guard::And(list) => {
            for g in list {
                if !evaluate_guard(cx, g, event)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Guard::Or(list) => {
            for g in list {
                if evaluate_guard(cx, g, event)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}
