//! Pure transition API: compute the next state and the actions that *would* run, without
//! executing user logic or side effects (XState's `transition()` / `initialTransition()`,
//! the Python library's `pure_transition` / `initial_transition`).
//!
//! Guards are evaluated (they must be registered), built-in `assign` is applied so the
//! returned context is exact, and every user action is reported as a [`PlannedAction`]
//! instead of being called. Timers, services and emitted events are discarded.

use std::sync::Arc;

use serde_json::Value;

use crate::context::Context;
use crate::engine::{self, ActiveState};
use crate::error::Result;
use crate::event::Event;
use crate::logic::MachineLogic;
use crate::machine::Machine;

/// A user action the engine would have executed.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedAction {
    /// Registry name.
    pub name: Arc<str>,
    /// Static `params` (or `null`).
    pub params: Value,
}

/// Compute the initial state and the entry actions that would run.
///
/// # Errors
/// Missing guard implementations or context construction failures.
pub fn initial_transition<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    input: &Value,
) -> Result<(ActiveState<C>, Vec<PlannedAction>)> {
    let (state, _effects, report) = engine::start_with(machine, logic, input, true)?;
    Ok((state, report.planned))
}

/// Compute the state after `event` and the actions that would run, leaving `state` untouched.
///
/// # Errors
/// Missing guard implementations; `maxIterations` exhaustion.
#[allow(clippy::needless_pass_by_value)] // public signature; the step borrows the event
pub fn transition<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    state: &ActiveState<C>,
    event: Event,
) -> Result<(ActiveState<C>, Vec<PlannedAction>)> {
    let mut next = state.clone();
    let mut effects = Vec::new();
    let report = engine::step_with(machine, logic, &mut next, &event, &mut effects, true)?;
    Ok((next, report.planned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_actions_without_running_them() {
        let machine = Machine::from_json(
            r#"{"id":"m","initial":"a","context":{"n":0},"states":{
                "a":{"entry":"enterA","on":{"GO":{"target":"b","actions":["doIt",{"type":"assign","params":{"assignment":{"n":1}}}]}}},
                "b":{"entry":"enterB"}}}"#,
        )
        .expect("machine");
        let logic = MachineLogic::dynamic().action("doIt", |_| panic!("must not run"));
        let (start, planned) = initial_transition(&machine, &logic, &Value::Null).expect("initial");
        assert_eq!(planned.iter().map(|a| &*a.name).collect::<Vec<_>>(), ["enterA"]);
        let (next, planned) = transition(&machine, &logic, &start, Event::new("GO")).expect("step");
        assert_eq!(planned.iter().map(|a| &*a.name).collect::<Vec<_>>(), ["doIt", "enterB"]);
        assert_eq!(next.leaf_ids(&machine), ["m.b"]);
        assert_eq!(next.context()["n"], 1);
        assert_eq!(start.leaf_ids(&machine), ["m.a"], "input state is untouched");
    }
}
