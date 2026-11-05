//! The compiled, immutable machine: an arena of state nodes in document (pre-)order with
//! every target resolved, every event descriptor normalised and every built-in action
//! recognised. Built once, shared by any number of interpreters.

use std::collections::HashMap;
use std::sync::Arc;

use foldhash::HashMap as FastMap;

use serde_json::Value;
use smallvec::SmallVec;

use crate::config::{ConfigWarning, MachineConfig, ParseOptions, Policies};
use crate::error::Result;
use crate::ids::{StateId, TransitionId};

pub(crate) mod builtin;
mod compile;

pub use builtin::{Builtin, Delay, SendTarget};

/// A compiled statechart. Cheap to clone (it is an `Arc` inside).
#[derive(Debug, Clone)]
pub struct Machine {
    inner: Arc<MachineData>,
}

#[derive(Debug)]
pub(crate) struct MachineData {
    pub(crate) id: String,
    pub(crate) states: Vec<StateNode>,
    pub(crate) transitions: Vec<Transition>,
    pub(crate) ids: FastMap<Arc<str>, StateId>,
    pub(crate) custom_ids: HashMap<String, StateId>,
    pub(crate) context: Value,
    pub(crate) version: Option<String>,
    pub(crate) policies: Policies,
    pub(crate) warnings: Vec<ConfigWarning>,
    pub(crate) has_always: bool,
    pub(crate) declared_events: Vec<String>,
    /// Exact event descriptor → interned key used by every node's `on_exact` table.
    pub(crate) event_index: FastMap<String, u32>,
    pub(crate) hash: std::sync::OnceLock<String>,
    /// The configuration document as parsed, returned by [`Machine::to_json`].
    pub(crate) source: Value,
}

/// Kind of a compiled state node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// Leaf state.
    Atomic,
    /// Exactly one child active at a time.
    Compound {
        /// The `initial` child.
        initial: StateId,
    },
    /// Every child (region) active at once.
    Parallel,
    /// Terminal state of its parent.
    Final,
    /// History pseudo-state.
    History {
        /// Restore leaves (`deep`) or direct children.
        deep: bool,
        /// Fallback when nothing was recorded (`target`, else the parent's `initial`).
        target: Option<StateId>,
    },
}

/// A compiled state node.
#[derive(Debug)]
pub struct StateNode {
    /// Full dotted id, e.g. `machine.a.b`.
    pub id: Arc<str>,
    /// Key under the parent's `states`.
    pub key: smol_str::SmolStr,
    /// Parent, `None` for the root.
    pub parent: Option<StateId>,
    /// Children in document order.
    pub children: Vec<StateId>,
    /// Depth from the root (root is 0).
    pub depth: u32,
    /// Node kind.
    pub kind: NodeKind,
    /// Exclusive end of this node's subtree in the pre-order arena.
    pub(crate) subtree_end: u32,
    /// Exact-descriptor transition lookup, descriptors already normalised to v5 names.
    /// Exact-descriptor table: (interned event key, transitions). Linear scan — nodes rarely
    /// declare more than a handful of events, and this beats hashing for them.
    pub(crate) on_exact: SmallVec<[(u32, SmallVec<[TransitionId; 2]>); 4]>,
    /// Partial wildcard descriptors (`foo.*`) longest first, with their transitions.
    pub(crate) on_partial: Vec<(String, SmallVec<[TransitionId; 2]>)>,
    /// `*` transitions.
    pub(crate) on_wildcard: SmallVec<[TransitionId; 2]>,
    /// Eventless transitions.
    pub(crate) always: SmallVec<[TransitionId; 2]>,
    /// Entry actions.
    pub entry: Vec<ActionDef>,
    /// Exit actions.
    pub exit: Vec<ActionDef>,
    /// Invoked services.
    pub invokes: Vec<InvokeDef>,
    /// Delayed transitions armed on entry.
    pub after: Vec<AfterDef>,
    /// Python-dialect `initial` key for the structure hash (compound states only).
    pub(crate) initial_key: Option<String>,
    /// `output` of a final state.
    pub output: Option<Value>,
    /// Free-form metadata.
    pub meta: Option<Value>,
    /// Human description.
    pub description: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
    /// History children, for recording on exit.
    pub(crate) history_children: SmallVec<[StateId; 1]>,
}

/// What triggers a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A named event descriptor (exact, partial wildcard or `*`), v5-normalised.
    Event(smol_str::SmolStr),
    /// Eventless (`always`).
    Always,
}

/// A compiled transition.
#[derive(Debug)]
pub struct Transition {
    /// Owning state.
    pub source: StateId,
    /// Trigger.
    pub trigger: Trigger,
    /// Resolved targets; empty for targetless transitions.
    pub targets: Vec<StateId>,
    /// Targets as written in the configuration (kept for the structure hash).
    pub raw_targets: SmallVec<[smol_str::SmolStr; 1]>,
    /// Actions in order.
    pub actions: Vec<ActionDef>,
    /// Guard expression.
    pub guard: Option<Guard>,
    /// `reenter`.
    pub reenter: bool,
    /// Forbidden transition (`null` / `{}`): consumes the event, does nothing.
    pub forbidden: bool,
    /// Free-form metadata.
    pub meta: Option<Value>,
    /// Human description.
    pub description: Option<String>,
}

/// A compiled action reference.
#[derive(Debug, Clone)]
pub enum ActionDef {
    /// Implemented by the logic registry.
    User {
        /// Registry name.
        name: Arc<str>,
        /// `params` (or `null`).
        params: Value,
    },
    /// An XState built-in.
    Builtin {
        /// The spelling used in the configuration (kept for the structure hash).
        name: Arc<str>,
        /// Compiled built-in.
        builtin: Builtin,
        /// `params` as written (or `null`), handed to a user action of the same name, which
        /// wins over the built-in when the logic implements it.
        params: Value,
    },
}

/// A compiled guard expression.
#[derive(Debug, Clone)]
pub enum Guard {
    /// Implemented by the logic registry.
    Named {
        /// Registry name.
        name: String,
        /// `params` (or `null`).
        params: Value,
    },
    /// Built-in `stateIn`: true when the given state is in the configuration.
    StateIn {
        /// Resolved state.
        state: StateId,
        /// The state reference as written (kept for the structure hash).
        raw: String,
    },
    /// Negation.
    Not(Box<Guard>),
    /// Conjunction.
    And(Vec<Guard>),
    /// Disjunction.
    Or(Vec<Guard>),
}

/// A compiled `invoke`.
#[derive(Debug, Clone)]
pub struct InvokeDef {
    /// Invoke id (explicit or the owning state's id).
    pub id: String,
    /// Logic name.
    pub src: String,
    /// Static input.
    pub input: Value,
    /// `systemId`.
    pub system_id: Option<String>,
}

/// A compiled `after` entry.
#[derive(Debug, Clone)]
pub struct AfterDef {
    /// Normalised key: whole milliseconds (`"1000"` for `"1e3"`) or the delay name.
    pub key: String,
    /// The key as written in the configuration.
    pub raw_key: String,
    /// Fixed or named delay.
    pub delay: Delay,
    /// Event type the timer raises (`xstate.after.<key>.<state id>`).
    pub event: String,
}

impl Machine {
    /// Parse and compile an XState JSON document.
    ///
    /// # Errors
    /// Shape errors from parsing and structural errors from compilation.
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)?;
        Self::from_value(&value)
    }

    /// Parse and compile an XState JSON value.
    ///
    /// # Errors
    /// Shape errors from parsing and structural errors from compilation.
    pub fn from_value(value: &Value) -> Result<Self> {
        Self::from_config(MachineConfig::from_value(value)?)
    }

    /// Parse with options and compile.
    ///
    /// # Errors
    /// Shape errors from parsing and structural errors from compilation.
    pub fn from_value_with(value: &Value, options: &ParseOptions) -> Result<Self> {
        Self::from_config(MachineConfig::from_value_with(value, options)?)
    }

    /// Compile a parsed configuration.
    ///
    /// # Errors
    /// Unresolved targets, root targets, duplicate ids and other structural rules.
    pub fn from_config(config: MachineConfig) -> Result<Self> {
        Ok(Self { inner: Arc::new(compile::compile(config, None)?) })
    }

    /// Parse and compile for a known logic registry: an action the logic implements is the
    /// user's action even when it is spelled like a built-in (`log`, `cancel`, `stop`, ...),
    /// so its own `params` are not checked against the built-in's (user logic wins over
    /// built-ins, as in XState, where a string action type names an implementation).
    ///
    /// # Errors
    /// Shape errors from parsing and structural errors from compilation.
    pub fn from_value_with_logic<C: crate::Context>(
        value: &Value,
        options: &ParseOptions,
        logic: &crate::MachineLogic<C>,
    ) -> Result<Self> {
        let names: std::collections::HashSet<String> =
            logic.action_names().map(str::to_owned).collect();
        let config = MachineConfig::from_value_for(value, options, &names)?;
        Ok(Self { inner: Arc::new(compile::compile(config, Some(&names))?) })
    }

    /// The configuration document this machine was compiled from: the input verbatim for
    /// [`Machine::from_value`] / [`Machine::from_json`], the equivalent document for machines
    /// built programmatically. Feeding it back to [`Machine::from_value`] yields an equivalent
    /// machine.
    #[must_use]
    pub fn to_json(&self) -> Value {
        self.inner.source.clone()
    }

    /// [`Machine::to_json`] pretty-printed (two-space indent).
    #[must_use]
    pub fn to_json_string(&self) -> String {
        // Serialising a `Value` cannot fail: every key is a string.
        serde_json::to_string_pretty(&self.inner.source).unwrap_or_default()
    }

    /// Mermaid `stateDiagram-v2` rendering. See [`crate::export`].
    #[must_use]
    pub fn to_mermaid(&self) -> String {
        crate::export::to_mermaid(self)
    }

    /// `PlantUML` rendering. See [`crate::export`].
    #[must_use]
    pub fn to_plantuml(&self) -> String {
        crate::export::to_plantuml(self)
    }

    /// Machine id (the root key).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Chart `version` label.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.inner.version.as_deref()
    }

    /// Initial context value as written (`null` when absent).
    #[must_use]
    pub fn initial_context(&self) -> &Value {
        &self.inner.context
    }

    /// Runtime policies.
    #[must_use]
    pub fn policies(&self) -> &Policies {
        &self.inner.policies
    }

    /// Parse-time warnings.
    #[must_use]
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.inner.warnings
    }

    /// Machine-level `output` (the root's `output`).
    #[must_use]
    pub fn output(&self) -> Option<&Value> {
        self.inner.states[0].output.as_ref()
    }

    /// Every state in document order.
    #[must_use]
    pub fn states(&self) -> &[StateNode] {
        &self.inner.states
    }

    /// Node by id.
    #[must_use]
    pub fn node(&self, id: StateId) -> &StateNode {
        &self.inner.states[id.index()]
    }

    /// Transition by id.
    #[must_use]
    pub fn transition(&self, id: TransitionId) -> &Transition {
        &self.inner.transitions[id.index()]
    }

    /// Every transition.
    #[must_use]
    pub fn transitions(&self) -> &[Transition] {
        &self.inner.transitions
    }

    /// Look a state up by its full dotted id (`machine.a.b`) or custom `#id` (without `#`),
    /// optionally followed by a relative path (`my.id.child`). Ids may contain dots; the
    /// longest dotted prefix naming a custom id wins.
    #[must_use]
    pub fn state_by_id(&self, id: &str) -> Option<StateId> {
        if let Some(&sid) = self.inner.ids.get(id) {
            return Some(sid);
        }
        let descend = |scope: StateId, path: &str| {
            path.split('.').try_fold(scope, |cur, key| {
                self.node(cur).children.iter().copied().find(|&c| self.node(c).key == key)
            })
        };
        compile::resolve_hash_id(&self.inner.ids, &self.inner.custom_ids, id, descend)
    }

    /// Python-compatible structure hash (16 hex chars), computed once per machine. See
    /// [`crate::snapshot::machine_hash`].
    #[must_use]
    pub fn structure_hash(&self) -> &str {
        self.inner.hash.get_or_init(|| crate::snapshot::machine_hash(self))
    }

    /// Interned key of an exact event descriptor, if any node declares it.
    #[inline]
    #[must_use]
    pub fn event_key(&self, event_type: &str) -> Option<u32> {
        self.inner.event_index.get(event_type).copied()
    }

    /// Event types declared on any state (exact descriptors only), for strict mode.
    #[must_use]
    pub fn declared_events(&self) -> &[String] {
        &self.inner.declared_events
    }

    /// Whether any state has eventless transitions (lets the engine skip settling).
    #[must_use]
    pub fn has_always(&self) -> bool {
        self.inner.has_always
    }

    /// `true` when `node` is a proper descendant of `ancestor`.
    #[must_use]
    pub fn is_descendant(&self, node: StateId, ancestor: StateId) -> bool {
        let a = &self.inner.states[ancestor.index()];
        node.0 > ancestor.0 && node.0 < a.subtree_end
    }

    /// `true` when `node == ancestor` or `node` is a proper descendant of `ancestor`.
    #[must_use]
    pub fn is_self_or_descendant(&self, node: StateId, ancestor: StateId) -> bool {
        node == ancestor || self.is_descendant(node, ancestor)
    }

    /// Proper ancestors of `node`, nearest first, stopping before `until` (exclusive).
    pub fn ancestors(
        &self,
        node: StateId,
        until: Option<StateId>,
    ) -> impl Iterator<Item = StateId> + '_ {
        let mut cur = self.inner.states[node.index()].parent;
        std::iter::from_fn(move || {
            let next = cur?;
            if Some(next) == until {
                return None;
            }
            cur = self.inner.states[next.index()].parent;
            Some(next)
        })
    }

    /// Least common ancestor of a non-empty set that is a *proper* ancestor of every member
    /// (SCXML `findLCCA`). Returns the root when nothing else qualifies.
    #[must_use]
    pub fn lcca(&self, nodes: &[StateId]) -> StateId {
        let Some(&first) = nodes.first() else { return StateId::ROOT };
        for anc in self.ancestors(first, None) {
            if nodes.iter().all(|&n| self.is_descendant(n, anc)) {
                return anc;
            }
        }
        StateId::ROOT
    }

    /// Atomic (leaf) descendants of `node` in document order, including `node` itself when it
    /// is atomic or final.
    #[must_use]
    pub fn leaves(&self, node: StateId) -> Vec<StateId> {
        let n = &self.inner.states[node.index()];
        (node.0..n.subtree_end)
            .map(StateId)
            .filter(|s| {
                matches!(self.inner.states[s.index()].kind, NodeKind::Atomic | NodeKind::Final)
            })
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn data(&self) -> &MachineData {
        &self.inner
    }
}

impl StateNode {
    /// `true` for `final` states.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(self.kind, NodeKind::Final)
    }

    /// `true` for history pseudo-states.
    #[must_use]
    pub fn is_history(&self) -> bool {
        matches!(self.kind, NodeKind::History { .. })
    }

    /// `true` for atomic and final states (the ones that appear in `state_ids`).
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        matches!(self.kind, NodeKind::Atomic | NodeKind::Final)
    }

    /// `true` for parallel states.
    #[must_use]
    pub fn is_parallel(&self) -> bool {
        matches!(self.kind, NodeKind::Parallel)
    }

    /// `true` for compound states.
    #[must_use]
    pub fn is_compound(&self) -> bool {
        matches!(self.kind, NodeKind::Compound { .. })
    }

    /// Candidate transitions for an event in XState v5 order: the exact descriptor's
    /// transitions first, then matching partial wildcards (longest first), then `*`. Wildcard
    /// candidates are appended even when an exact descriptor exists, so a failed exact guard
    /// falls through to them. `key` is the interned event key (see [`Machine::event_key`]).
    #[inline]
    pub(crate) fn candidates<'a>(
        &'a self,
        key: Option<u32>,
        event_type: &'a str,
    ) -> impl Iterator<Item = TransitionId> + 'a {
        let exact = key
            .and_then(|k| self.on_exact.iter().find(|(ek, _)| *ek == k))
            .into_iter()
            .flat_map(|(_, ts)| ts.iter().copied());
        let partial = self
            .on_partial
            .iter()
            .filter(move |(desc, _)| partial_matches(desc, event_type))
            .flat_map(|(_, ts)| ts.iter().copied());
        exact.chain(partial).chain(self.on_wildcard.iter().copied())
    }
}

/// XState v5 partial descriptor matching: `a.b.*` matches `a.b` and `a.b.anything...`.
pub(crate) fn partial_matches(descriptor: &str, event_type: &str) -> bool {
    let mut pattern = descriptor.split('.').peekable();
    let mut tokens = event_type.split('.');
    loop {
        let Some(p) = pattern.next() else { return true };
        if p == "*" {
            return pattern.peek().is_none();
        }
        match tokens.next() {
            Some(t) if t == p => {}
            _ => return false,
        }
    }
}
