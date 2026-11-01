//! The explorer: candidate discovery and one-step successors over the pure API.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{GraphOptions, GuardPolicy, UNKNOWN_DELAY_MS, WILDCARD_PROBE, stub};
use crate::context::Context;
use crate::engine::{ActiveState, Status};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::logic::MachineLogic;
use crate::machine::{Delay, Machine, Trigger};
use crate::pure;

/// Identity of a configuration: sorted leaf ids plus (optionally) the canonical context.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConfigKey {
    leaves: Vec<String>,
    context: String,
}

/// An initial state under one guard assumption.
pub(crate) struct Root<C> {
    pub(crate) guards: bool,
    pub(crate) state: ActiveState<C>,
    pub(crate) key: ConfigKey,
}

/// One step out of a configuration.
pub(crate) struct Edge<C> {
    pub(crate) event: Event,
    pub(crate) guards: bool,
    pub(crate) delay_ms: Option<f64>,
    pub(crate) state: ActiveState<C>,
    pub(crate) key: ConfigKey,
}

/// An event to try, with the simulated time it consumes (timers only).
struct Candidate {
    event: Event,
    delay_ms: Option<f64>,
}

/// A logic with every guard forced to one value.
struct Variant<C> {
    guards: bool,
    logic: MachineLogic<C>,
}

/// Drives the machine one step at a time under the configured guard assumptions.
pub(crate) struct Explorer<'a, C> {
    machine: &'a Machine,
    options: &'a GraphOptions,
    variants: Vec<Variant<C>>,
    /// Concrete event types per state that its own transitions handle.
    probes: Vec<Vec<String>>,
    refused: usize,
}

impl<'a, C: Context> Explorer<'a, C> {
    pub(crate) fn new(
        machine: &'a Machine,
        logic: &MachineLogic<C>,
        options: &'a GraphOptions,
    ) -> Self {
        let guard_names = stub::collect_names(machine).guards;
        let values: &[bool] = match options.guards {
            GuardPolicy::True => &[true],
            GuardPolicy::False => &[false],
            GuardPolicy::Both => &[true, false],
        };
        let variants = values
            .iter()
            .map(|&guards| Variant {
                guards,
                logic: stub::with_constant_guards(logic.clone(), &guard_names, guards),
            })
            .collect();
        Self { machine, options, variants, probes: handler_probes(machine), refused: 0 }
    }

    pub(crate) fn options(&self) -> &'a GraphOptions {
        self.options
    }

    /// Steps the engine refused so far.
    pub(crate) fn refused(&self) -> usize {
        self.refused
    }

    /// Distinct initial states, one per guard assumption.
    pub(crate) fn roots(&self) -> Result<Vec<Root<C>>> {
        let mut roots: Vec<Root<C>> = Vec::new();
        for variant in &self.variants {
            let (state, _) =
                pure::initial_transition(self.machine, &variant.logic, &self.options.input)?;
            let key = self.key_of(&state)?;
            if roots.iter().all(|r| r.key != key) {
                roots.push(Root { guards: variant.guards, state, key });
            }
        }
        Ok(roots)
    }

    /// Every distinct configuration reachable from `state` in one step, in candidate order.
    ///
    /// Steps that change nothing (self loops) are dropped, as are steps the engine refuses
    /// (counted in [`Explorer::refused`]). A missing implementation is a caller error and
    /// propagates.
    pub(crate) fn successors(&mut self, state: &ActiveState<C>) -> Result<Vec<Edge<C>>> {
        let from = self.key_of(state)?;
        let mut edges = Vec::new();
        for candidate in self.candidates(state) {
            let mut seen: Vec<ConfigKey> = Vec::new();
            for variant in &self.variants {
                let outcome =
                    pure::transition(self.machine, &variant.logic, state, candidate.event.clone());
                let next = match outcome {
                    Ok((next, _planned)) => next,
                    Err(err @ Error::ImplementationMissing { .. }) => return Err(err),
                    Err(_) => {
                        self.refused += 1;
                        continue;
                    }
                };
                if next.status() == Status::Error {
                    continue;
                }
                let key = self.key_of(&next)?;
                if key == from || seen.contains(&key) {
                    continue;
                }
                seen.push(key.clone());
                edges.push(Edge {
                    event: candidate.event.clone(),
                    guards: variant.guards,
                    delay_ms: candidate.delay_ms,
                    state: next,
                    key,
                });
            }
        }
        Ok(edges)
    }

    fn key_of(&self, state: &ActiveState<C>) -> Result<ConfigKey> {
        let mut context = String::new();
        if self.options.key_by_context {
            canonical_json(&serde_json::to_value(state.context())?, &mut context);
        }
        Ok(ConfigKey { leaves: state.leaf_ids(self.machine), context })
    }

    /// Events, armed timers and service completions applicable in `state`.
    fn candidates(&self, state: &ActiveState<C>) -> Vec<Candidate> {
        let mut out = self.event_candidates(state);
        let fail_services = self.options.guards == GuardPolicy::Both;
        for id in state.configuration() {
            let node = self.machine.node(id);
            for after in &node.after {
                out.push(Candidate {
                    event: Event::after(&after.key, &node.id),
                    delay_ms: Some(self.delay_ms(&after.delay)),
                });
            }
            for invoke in &node.invokes {
                let done = Event::done_actor(&invoke.id, Value::Null);
                out.push(Candidate { event: done, delay_ms: None });
                if fail_services {
                    let error = Value::String("graph: service forced to error".to_owned());
                    let failed = Event::error_actor(&invoke.id, error);
                    out.push(Candidate { event: failed, delay_ms: None });
                }
            }
        }
        out
    }

    /// The user events to try: the caller's override, else whatever the active states handle.
    fn event_candidates(&self, state: &ActiveState<C>) -> Vec<Candidate> {
        let events: Vec<Event> = if let Some(events) = &self.options.events {
            events.clone()
        } else {
            let types: BTreeSet<&str> = state
                .configuration()
                .flat_map(|s| self.probes[s.index()].iter().map(String::as_str))
                .collect();
            types.into_iter().map(Event::new).collect()
        };
        events.into_iter().map(|event| Candidate { event, delay_ms: None }).collect()
    }

    fn delay_ms(&self, delay: &Delay) -> f64 {
        match delay {
            Delay::Fixed(ms) => *ms,
            Delay::Named(name) => {
                self.options.named_delays.get(name).copied().unwrap_or(UNKNOWN_DELAY_MS)
            }
        }
    }
}

/// Concrete event types each state's own transitions respond to.
fn handler_probes(machine: &Machine) -> Vec<Vec<String>> {
    let mut probes: Vec<BTreeSet<String>> = vec![BTreeSet::new(); machine.states().len()];
    for transition in machine.transitions() {
        if let Trigger::Event(descriptor) = &transition.trigger
            && let Some(probe) = probe_for(descriptor)
        {
            probes[transition.source.index()].insert(probe);
        }
    }
    probes.into_iter().map(|set| set.into_iter().collect()).collect()
}

/// A concrete user event a descriptor matches. Engine-generated descriptors (`xstate.*`:
/// timers, service results, completion) are driven separately and yield `None`.
fn probe_for(descriptor: &str) -> Option<String> {
    if descriptor.starts_with("xstate.") {
        None
    } else if descriptor == "*" {
        Some(WILDCARD_PROBE.to_owned())
    } else if let Some(prefix) = descriptor.strip_suffix('*') {
        Some(format!("{prefix}xsm_graph_any"))
    } else {
        Some(descriptor.to_owned())
    }
}

/// Write `value` with object keys sorted, so equal contexts built in different insertion
/// orders get the same identity.
fn canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical_json(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}
