//! Name discovery and constant-guard stand-ins: [`stub_logic`] and the guard overlay the
//! explorer applies on top of the caller's logic.

use std::collections::BTreeSet;

use serde_json::Value;

use super::GuardPolicy;
use crate::context::Context;
use crate::logic::MachineLogic;
use crate::machine::{ActionDef, Builtin, Delay, Guard, Machine};

/// Every implementation name a machine references, by kind.
#[derive(Debug, Default)]
pub(crate) struct Names {
    pub(crate) actions: BTreeSet<String>,
    pub(crate) guards: BTreeSet<String>,
    pub(crate) services: BTreeSet<String>,
    pub(crate) delays: BTreeSet<String>,
}

impl Names {
    fn actions(&mut self, actions: &[ActionDef]) {
        for action in actions {
            match action {
                ActionDef::User { name, .. } => {
                    self.actions.insert(name.to_string());
                }
                ActionDef::Builtin { builtin, .. } => self.builtin(builtin),
            }
        }
    }

    fn builtin(&mut self, builtin: &Builtin) {
        match builtin {
            Builtin::Raise { delay, .. } | Builtin::SendTo { delay, .. } => {
                self.delay(delay.as_ref());
            }
            Builtin::SpawnChild { src, .. } => {
                self.services.insert(src.clone());
            }
            Builtin::Choose(branches) => {
                for branch in branches {
                    if let Some(guard) = &branch.guard {
                        self.guard(guard);
                    }
                    self.actions(&branch.actions);
                }
            }
            _ => {}
        }
    }

    fn delay(&mut self, delay: Option<&Delay>) {
        if let Some(Delay::Named(name)) = delay {
            self.delays.insert(name.clone());
        }
    }

    fn guard(&mut self, guard: &Guard) {
        match guard {
            Guard::Named { name, .. } => {
                self.guards.insert(name.clone());
            }
            Guard::StateIn { .. } => {}
            Guard::Not(inner) => self.guard(inner),
            Guard::And(list) | Guard::Or(list) => list.iter().for_each(|g| self.guard(g)),
        }
    }
}

/// Collect every action, guard, service and delay name `machine` refers to. Built-in actions
/// (`assign`, `raise`, ...) are not user implementations and are excluded.
pub(crate) fn collect_names(machine: &Machine) -> Names {
    let mut names = Names::default();
    for node in machine.states() {
        names.actions(&node.entry);
        names.actions(&node.exit);
        for invoke in &node.invokes {
            names.services.insert(invoke.src.clone());
        }
        for after in &node.after {
            if let Delay::Named(name) = &after.delay {
                names.delays.insert(name.clone());
            }
        }
    }
    for transition in machine.transitions() {
        names.actions(&transition.actions);
        if let Some(guard) = &transition.guard {
            names.guard(guard);
        }
    }
    names
}

/// Register every name in `guards` as a constant guard returning `value`, replacing any
/// implementation `logic` already has under that name.
pub(crate) fn with_constant_guards<C: Context>(
    mut logic: MachineLogic<C>,
    guards: &BTreeSet<String>,
    value: bool,
) -> MachineLogic<C> {
    for name in guards {
        logic = logic.guard(name.clone(), move |_, _, _| Ok(value));
    }
    logic
}

/// A [`MachineLogic`] that satisfies every name `machine` references, mirroring the Python
/// library's `testing_utils.stub_logic`: constant guards, no-op actions, services returning
/// `null`, and zero-millisecond named delays.
///
/// [`GuardPolicy::True`] and [`GuardPolicy::False`] make every guard return that value.
/// [`GuardPolicy::Both`] has no single constant and registers `true` guards; the graph
/// functions derive the `false` variant themselves.
///
/// ```
/// use xstate_core::{DynContext, Machine};
/// use xstate_core::graph::{GuardPolicy, stub_logic};
///
/// let machine = Machine::from_json(r#"{
///   "id": "m", "initial": "a",
///   "states": { "a": { "on": { "GO": { "target": "b", "guard": "ok", "actions": "save" } } },
///               "b": {} }
/// }"#)?;
/// let logic = stub_logic::<DynContext>(&machine, GuardPolicy::True);
/// logic.validate(&machine)?;
/// # Ok::<(), xstate_core::Error>(())
/// ```
#[must_use]
pub fn stub_logic<C: Context>(machine: &Machine, guards: GuardPolicy) -> MachineLogic<C> {
    let names = collect_names(machine);
    let mut logic = MachineLogic::<C>::new();
    for name in names.actions {
        logic = logic.action(name, |_| Ok(()));
    }
    for name in names.services {
        logic = logic.service(name, |_, _| Ok(Value::Null));
    }
    for name in names.delays {
        logic = logic.delay(name, 0.0);
    }
    with_constant_guards(logic, &names.guards, !matches!(guards, GuardPolicy::False))
}
