//! Static coverage targets: the edges and states a coverage tool should see hit.

use std::collections::HashSet;

use crate::machine::{Machine, Trigger};

/// The event label [`CoverageTarget::event`] carries for an eventless (`always`) transition.
const ALWAYS_LABEL: &str = "always";

/// One transition of the chart, as a coverage tool sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CoverageTarget {
    /// Full id of the state that owns the transition.
    pub source: String,
    /// The event descriptor (`TOGGLE`, `foo.*`, `*`, `xstate.after.500.m.a`,
    /// `xstate.done.actor.fetch`, ...), or `always` for an eventless transition.
    pub event: String,
    /// Full ids of the targets; empty for a targetless transition.
    pub targets: Vec<String>,
}

/// Every `(source, event, targets)` edge of the chart, in document order, deduplicated.
///
/// Static: read from the compiled machine, nothing is executed. Guarded alternatives with the
/// same source, event and targets collapse into one entry; forbidden transitions are included
/// with no targets.
///
/// ```
/// use xstate_core::Machine;
/// use xstate_core::graph::transition_coverage_targets;
///
/// let machine = Machine::from_json(r#"{
///   "id": "m", "initial": "a",
///   "states": { "a": { "on": { "GO": "b" } }, "b": {} }
/// }"#)?;
/// let targets = transition_coverage_targets(&machine);
/// assert_eq!(targets.len(), 1);
/// assert_eq!(targets[0].targets, ["m.b"]);
/// # Ok::<(), xstate_core::Error>(())
/// ```
#[must_use]
pub fn transition_coverage_targets(machine: &Machine) -> Vec<CoverageTarget> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for transition in machine.transitions() {
        let event = match &transition.trigger {
            Trigger::Event(descriptor) => descriptor.to_string(),
            Trigger::Always => ALWAYS_LABEL.to_owned(),
        };
        let target = CoverageTarget {
            source: machine.node(transition.source).id.to_string(),
            event,
            targets: transition.targets.iter().map(|t| machine.node(*t).id.to_string()).collect(),
        };
        if seen.insert(target.clone()) {
            out.push(target);
        }
    }
    out
}

/// Every state id a coverage tool should expect to see entered: all states except the root
/// and history pseudo-states (which can never be active), in document order.
#[must_use]
pub fn state_coverage_targets(machine: &Machine) -> Vec<String> {
    machine
        .states()
        .iter()
        .skip(1)
        .filter(|node| !node.is_history())
        .map(|node| node.id.to_string())
        .collect()
}
