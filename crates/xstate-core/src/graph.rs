//! Graph utilities over the pure transition API: reachable configurations, shortest and
//! simple paths, and transition-coverage targets for model-based testing.
//!
//! The Rust counterpart of the Python library's `graph` module and of `@xstate/graph`'s
//! `getShortestPaths` / `getSimplePaths`. Every edge is discovered by running the *real*
//! engine through [`pure::transition`](crate::pure::transition), so selection, entry and exit
//! ordering, `always` settling, history, parallel regions and `onDone` behave exactly as they
//! do at run time; nothing here re-implements them.
//!
//! A *configuration* is the sorted set of active leaf state ids, optionally paired with the
//! context ([`GraphOptions::key_by_context`]). History records are not part of the identity.
//!
//! # What gets driven
//!
//! From each configuration the explorer tries, one at a time:
//!
//! * every event the active states (ancestors included) handle. Wildcard handlers (`*`,
//!   `mouse.*`) are exercised with a synthetic event ([`WILDCARD_PROBE`] for `*`);
//! * every armed `after` timer, delivered as its `xstate.after.<key>.<state id>` event;
//! * the completion of every invoked service (`xstate.done.actor.<id>`), plus its failure
//!   (`xstate.error.actor.<id>`) under [`GuardPolicy::Both`].
//!
//! Services are never executed by the pure API, which is why their results arrive as events.
//! [`GraphOptions::events`] replaces the first group with a caller-chosen event set.
//!
//! # Guards
//!
//! Guards are *assumed*, not evaluated: [`GraphOptions::guards`] overrides every guard the
//! machine names with a constant. [`GuardPolicy::Both`] explores each configuration twice
//! (all guards true, all guards false) and unions the results; every [`PathStep`] records the
//! assumption it relied on. Use [`stub_logic`] to obtain a logic that satisfies every name a
//! machine references.
//!
//! ```
//! use xstate_core::{DynContext, Machine};
//! use xstate_core::graph::{GraphOptions, GuardPolicy, reachable_states, shortest_paths, stub_logic};
//!
//! let machine = Machine::from_json(r#"{
//!   "id": "toggle", "initial": "off",
//!   "states": { "off": { "on": { "FLIP": "on" } }, "on": { "on": { "FLIP": "off" } } }
//! }"#)?;
//! let logic = stub_logic::<DynContext>(&machine, GuardPolicy::True);
//! let options = GraphOptions::default();
//! let report = reachable_states(&machine, &logic, &options)?;
//! assert!(report.unreachable.is_empty());
//! let paths = shortest_paths(&machine, &logic, &options)?;
//! assert_eq!(paths.iter().map(|p| p.steps.len()).collect::<Vec<_>>(), [0, 1]);
//! # Ok::<(), xstate_core::Error>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::context::Context;
use crate::engine::Status;
use crate::error::Result;
use crate::event::Event;
use crate::logic::MachineLogic;
use crate::machine::Machine;

mod explore;
mod search;
mod stub;
mod targets;

pub use stub::stub_logic;
pub use targets::{CoverageTarget, state_coverage_targets, transition_coverage_targets};

use explore::Explorer;

/// Duration, in milliseconds, assumed for a named delay with no entry in
/// [`GraphOptions::named_delays`] when weighing paths by time (the Python library's
/// `UNKNOWN_DELAY_MS`): large enough to lose against any realistic alternative.
pub const UNKNOWN_DELAY_MS: f64 = 1e9;

/// Event type sent to exercise a `*` handler; chosen so that no real event collides with it.
pub const WILDCARD_PROBE: &str = "xsm.graph.any";

/// How guards are treated while exploring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GuardPolicy {
    /// Every guard evaluates to `true` (the default).
    #[default]
    True,
    /// Every guard evaluates to `false`.
    False,
    /// Explore with all guards `true` and with all guards `false`, union the results, and
    /// also fail every invoked service. For [`stub_logic`] this registers `true` guards.
    Both,
}

/// What [`shortest_paths`] minimises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Weight {
    /// Number of steps (breadth-first search).
    #[default]
    Steps,
    /// Total `after` delay in milliseconds (Dijkstra); plain events weigh zero and ties are
    /// broken by step count.
    Time,
}

/// Options shared by [`reachable_states`], [`shortest_paths`] and [`simple_paths`].
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// Longest path, in steps, that is explored (default 50).
    pub max_depth: usize,
    /// [`simple_paths`] stops after this many paths (default 1000).
    pub max_paths: usize,
    /// Exploration fails with [`Error::LimitExceeded`](crate::Error::LimitExceeded) once more
    /// than this many configurations are found (default 100 000). Parallel regions multiply.
    pub max_configs: usize,
    /// Guard assumption; see [`GuardPolicy`].
    pub guards: GuardPolicy,
    /// Include the context in a configuration's identity (default `true`). With `false`, two
    /// visits to the same leaves with different contexts are one configuration.
    pub key_by_context: bool,
    /// Replace the events derived from the active states by this set, sent from every
    /// configuration. Timers and service completions are still driven.
    pub events: Option<Vec<Event>>,
    /// What [`shortest_paths`] minimises.
    pub weight: Weight,
    /// Durations of named delays, for [`Weight::Time`] and [`PathStep::delay_ms`]; names not
    /// listed weigh [`UNKNOWN_DELAY_MS`].
    pub named_delays: BTreeMap<String, f64>,
    /// Actor input handed to the initial context (`null` by default).
    pub input: Value,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            max_depth: 50,
            max_paths: 1000,
            max_configs: 100_000,
            guards: GuardPolicy::True,
            key_by_context: true,
            events: None,
            weight: Weight::Steps,
            named_delays: BTreeMap::new(),
            input: Value::Null,
        }
    }
}

/// One edge of a [`Path`]: an event delivered and the configuration it led to.
#[derive(Debug, Clone, PartialEq)]
pub struct PathStep {
    /// The event delivered. Timer steps carry the `xstate.after.*` event.
    pub event: Event,
    /// Active leaf state ids after the step, sorted.
    pub state_ids: Vec<String>,
    /// Context after the step.
    pub context: Value,
    /// Value every guard was assumed to have during this step.
    pub guards: bool,
    /// Milliseconds of simulated time the step consumes: `Some` for `after` timers, `None`
    /// for ordinary events.
    pub delay_ms: Option<f64>,
}

/// A replayable event sequence from an initial state to a configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    /// The steps, first to last; empty for the initial configuration.
    pub steps: Vec<PathStep>,
    /// Active leaf state ids at the end of the path, sorted.
    pub state_ids: Vec<String>,
    /// Context at the end of the path.
    pub context: Value,
    /// Guard assumption in force while entering the initial state (relevant for `always`
    /// transitions that settle at start-up under [`GuardPolicy::Both`]).
    pub initial_guards: bool,
}

impl Path {
    /// Total simulated time of the path: the sum of every timer step's delay.
    #[must_use]
    pub fn total_delay_ms(&self) -> f64 {
        self.steps.iter().filter_map(|s| s.delay_ms).fold(0.0, |total, ms| total + ms)
    }

    /// Compact rendering: event types joined by `,`, timer steps as `+<ms>`
    /// (`"SUBMIT,+2000,PAY"`), the Python `Path.event_string()` grammar.
    #[must_use]
    pub fn event_string(&self) -> String {
        let parts: Vec<String> = self
            .steps
            .iter()
            .map(|s| s.delay_ms.map_or_else(|| s.event.ty.to_string(), |ms| format!("+{ms}")))
            .collect();
        parts.join(",")
    }
}

/// One configuration visited by [`reachable_states`].
#[derive(Debug, Clone, PartialEq)]
pub struct Configuration {
    /// Active leaf state ids, sorted.
    pub state_ids: Vec<String>,
    /// Context in this configuration (the first one found when contexts are not part of the
    /// identity).
    pub context: Value,
    /// Engine status (`Done` for configurations ending in a top-level final state).
    pub status: Status,
    /// Fewest steps needed to reach it.
    pub depth: usize,
}

/// Result of [`reachable_states`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReachabilityReport {
    /// Every state id (ancestors of active leaves included) active in some configuration.
    pub reachable: BTreeSet<String>,
    /// State ids never active within [`GraphOptions::max_depth`], in document order.
    /// History pseudo-states are not reported: they cannot be active. A state that is only
    /// ever entered transiently (its `always` transitions settle away within the same
    /// macrostep) is never part of a stable configuration and is reported here too.
    pub unreachable: Vec<String>,
    /// Every configuration found, in breadth-first discovery order (initial first).
    pub configurations: Vec<Configuration>,
    /// Steps the engine refused (a runaway `always` loop, a guard that raised under the
    /// `raise` policy, ...). Refused edges are absent from the result.
    pub refused_steps: usize,
}

/// Explore breadth-first from the initial state and report what is reachable.
///
/// `logic` supplies the implementations the engine looks up (named delays in particular);
/// guards are overridden per [`GraphOptions::guards`]. See the [module docs](self) for what is
/// driven.
///
/// # Errors
/// Missing implementations ([`stub_logic`] provides them all), context construction failures
/// and exceeding [`GraphOptions::max_configs`].
pub fn reachable_states<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    options: &GraphOptions,
) -> Result<ReachabilityReport> {
    let mut explorer = Explorer::new(machine, logic, options);
    let arena = search::bfs(&mut explorer)?;
    let mut reachable = BTreeSet::new();
    let mut configurations = Vec::with_capacity(arena.len());
    for node in arena.nodes() {
        reachable.extend(node.state.configuration().map(|s| machine.node(s).id.to_string()));
        configurations.push(Configuration {
            state_ids: node.state.leaf_ids(machine),
            context: serde_json::to_value(node.state.context())?,
            status: node.state.status(),
            depth: node.depth,
        });
    }
    let unreachable = machine
        .states()
        .iter()
        .filter(|n| !n.is_history() && !reachable.contains(&*n.id))
        .map(|n| n.id.to_string())
        .collect();
    Ok(ReachabilityReport {
        reachable,
        unreachable,
        configurations,
        refused_steps: explorer.refused(),
    })
}

/// One shortest path per reachable configuration, the initial configuration (empty path)
/// first.
///
/// Breadth-first ([`Weight::Steps`]) yields paths in discovery order; [`Weight::Time`] runs
/// Dijkstra over `after` delays and yields them in order of increasing cost.
///
/// # Errors
/// As [`reachable_states`].
pub fn shortest_paths<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    options: &GraphOptions,
) -> Result<Vec<Path>> {
    let mut explorer = Explorer::new(machine, logic, options);
    let arena = match options.weight {
        Weight::Steps => search::bfs(&mut explorer)?,
        Weight::Time => search::dijkstra(&mut explorer)?,
    };
    (0..arena.len()).map(|i| arena.path(machine, i)).collect()
}

/// Every non-empty path that never revisits a configuration, depth first.
///
/// Stops after [`GraphOptions::max_paths`] paths and never exceeds
/// [`GraphOptions::max_depth`] steps. A path that ends in a `Done` configuration is reported
/// but not extended.
///
/// # Errors
/// Missing implementations and context construction failures.
pub fn simple_paths<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    options: &GraphOptions,
) -> Result<Vec<Path>> {
    let mut explorer = Explorer::new(machine, logic, options);
    let (arena, found) = search::dfs(&mut explorer)?;
    found.into_iter().map(|i| arena.path(machine, i)).collect()
}
