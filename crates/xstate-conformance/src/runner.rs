//! Runner: drives a [`Case`] through the Rust engine with the same stubbing the oracles use
//! and produces [`Observation`]s that can be compared with `expected.xstate`.

use std::sync::Arc;

use serde_json::Value;
use xstate_core::{
    DynContext, Error, Event, Machine, MachineLogic, SimulatedClock, Status, SyncInterpreter,
};

use crate::{Case, Observation, Step};

/// Result of running a case through the Rust engine.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Observation after `start()`.
    pub initial: Observation,
    /// One observation per step.
    pub steps: Vec<Observation>,
}

/// Build the stub logic a case declares: constant guards, services that return a fixed result
/// or fail with a fixed message, named delays, and no-op user actions for every action name
/// the machine references.
#[must_use]
pub fn stub_logic(case: &Case, machine: &Machine) -> MachineLogic<DynContext> {
    let mut logic = MachineLogic::new();
    for (name, value) in &case.logic.guards {
        let v = *value;
        logic = logic.guard(name.clone(), move |_, _, _| Ok(v));
    }
    for (name, stub) in &case.logic.services {
        let result = stub.result.clone();
        let error = stub.error.clone();
        logic = logic.service(name.clone(), move |_, _| match &error {
            Some(message) => Err(Error::logic(message)),
            None => Ok(result.clone().unwrap_or(Value::Null)),
        });
    }
    for (name, ms) in &case.logic.delays {
        logic = logic.delay(name.clone(), *ms);
    }
    for name in referenced_names(machine) {
        if !case.logic.guards.contains_key(&name) {
            logic = logic.guard(name.clone(), |_, _, _| Ok(true));
        }
        logic = logic.action(name.clone(), |_| Ok(()));
    }
    for name in referenced_services(machine) {
        if !case.logic.services.contains_key(&name) {
            logic = logic.service(name, |_, _| Ok(Value::Null));
        }
    }
    logic
}

fn referenced_names(machine: &Machine) -> Vec<String> {
    use xstate_core::machine::{ActionDef, Guard};
    fn guard_names(g: &Guard, out: &mut Vec<String>) {
        match g {
            Guard::Named { name, .. } => out.push(name.clone()),
            Guard::StateIn { .. } => {}
            Guard::Not(inner) => guard_names(inner, out),
            Guard::And(list) | Guard::Or(list) => list.iter().for_each(|g| guard_names(g, out)),
        }
    }
    fn action_names(actions: &[ActionDef], out: &mut Vec<String>) {
        for a in actions {
            match a {
                ActionDef::User { name, .. } => out.push(name.clone()),
                ActionDef::Builtin {
                    builtin: xstate_core::machine::Builtin::Choose(branches),
                    ..
                } => {
                    for b in branches {
                        if let Some(g) = &b.guard {
                            guard_names(g, out);
                        }
                        action_names(&b.actions, out);
                    }
                }
                ActionDef::Builtin { .. } => {}
            }
        }
    }
    let mut out = Vec::new();
    for node in machine.states() {
        action_names(&node.entry, &mut out);
        action_names(&node.exit, &mut out);
    }
    for t in machine.transitions() {
        action_names(&t.actions, &mut out);
        if let Some(g) = &t.guard {
            guard_names(g, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn referenced_services(machine: &Machine) -> Vec<String> {
    let mut out: Vec<String> =
        machine.states().iter().flat_map(|n| n.invokes.iter().map(|i| i.src.clone())).collect();
    out.sort();
    out.dedup();
    out
}

/// Run one case through the Rust engine.
///
/// # Errors
/// Returns the engine error if the machine fails to compile or a step fails unexpectedly.
pub fn run_case(case: &Case) -> Result<RunOutcome, Error> {
    run_case_with_interpreter(case).map(|(outcome, _)| outcome)
}

/// Run one case and also hand back the interpreter in its final state (for snapshot checks).
///
/// # Errors
/// Returns the engine error if the machine fails to compile or a step fails unexpectedly.
pub fn run_case_with_interpreter(
    case: &Case,
) -> Result<(RunOutcome, SyncInterpreter<DynContext>), Error> {
    let machine = Machine::from_value(&case.machine)?;
    let logic = stub_logic(case, &machine);
    let clock = SimulatedClock::new();
    let mut interp = SyncInterpreter::with_clock(machine, logic, Arc::new(clock.clone()));
    let started = interp.start()?;
    let initial = observe(&interp, started.actions);

    let mut steps = Vec::with_capacity(case.steps.len());
    for step in &case.steps {
        let actions = match step {
            Step::Send(value) => {
                let event = Event::from_json(value.clone())?;
                interp.send(event)?.actions
            }
            Step::AdvanceMs(ms) => interp.advance(&clock, *ms)?.actions,
        };
        steps.push(observe(&interp, actions));
    }
    Ok((RunOutcome { initial, steps }, interp))
}

fn observe(interp: &SyncInterpreter<DynContext>, actions: Vec<String>) -> Observation {
    let status = interp.status();
    let state = interp.state();
    Observation {
        state_ids: interp.state_ids(),
        context: interp.context().cloned().unwrap_or(Value::Null),
        actions,
        status: status.as_str().to_owned(),
        output: (status == Status::Done).then(|| state.map_or(Value::Null, |s| s.output().clone())),
        error: (status == Status::Error)
            .then(|| state.and_then(|s| s.error().map(str::to_owned)).unwrap_or_default()),
    }
}
