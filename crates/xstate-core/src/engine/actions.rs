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
/// [`Error::TransitionFailed`] for the caller to roll back or fail.
pub(super) fn run_actions<C: Context>(
    cx: &mut Ctx<'_, C>,
    actions: &[ActionDef],
    event: &Event,
) -> Result<()> {
    for action in actions {
        if let Err(err) = run_action(cx, action, event) {
            let name = action_name(action);
            match cx.machine.policies().action_error_policy {
                ActionErrorPolicy::Continue => {
                    tracing::warn!(target: "xstate", action = %name, error = %err, "action failed; continuing");
                    return Ok(());
                }
                ActionErrorPolicy::Rollback | ActionErrorPolicy::Fail => {
                    return Err(Error::TransitionFailed { action: name, source: Box::new(err) });
                }
            }
        }
    }
    Ok(())
}

fn action_name(action: &ActionDef) -> String {
    match action {
        ActionDef::User { name, .. } => name.clone(),
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
        ActionDef::User { name, params } => {
            let f = cx.logic.get_action(name)?.clone();
            let mut local: Vec<Effect> = Vec::new();
            {
                let mut scope = ActionScope {
                    context: &mut cx.state.context,
                    event,
                    params,
                    effects: &mut local,
                };
                f(&mut scope)?;
            }
            cx.report.actions.push(name.clone());
            cx.report.changed = true;
            for effect in local {
                match effect {
                    Effect::Raise(ev) => cx.state.internal_queue.push_back(ev),
                    other => cx.effects.push(other),
                }
            }
            Ok(())
        }
        ActionDef::Builtin { builtin, .. } => run_builtin(cx, builtin, event),
    }
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
                    let delay_ms = super::resolve_delay(cx.logic, d, &cx.state.context, event)?;
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
                Some(d) => Some(super::resolve_delay(cx.logic, d, &cx.state.context, event)?),
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
        Builtin::StopChild(id) => cx.effects.push(Effect::StopActor(id.clone())),
        Builtin::SpawnChild { src, id, system_id, input } => cx.effects.push(Effect::Spawn {
            src: src.clone(),
            id: id.clone().unwrap_or_else(|| src.clone()),
            system_id: system_id.clone(),
            input: input.clone(),
        }),
        Builtin::Emit(ev) => cx.effects.push(Effect::Emit(ev.clone())),
        Builtin::Choose(branches) => {
            for branch in branches {
                let taken = match &branch.guard {
                    None => true,
                    Some(g) => evaluate_guard(cx, g, event)?,
                };
                if taken {
                    return run_actions(cx, &branch.actions, event);
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
            let f = cx.logic.get_guard(name)?.clone();
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
