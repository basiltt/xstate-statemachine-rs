//! The raw configuration model: a faithful, ordered representation of an XState v5 JSON
//! machine, parsed leniently (shorthands expanded, v4 aliases accepted) but strictly typed.
//!
//! Parsing is deliberately separate from compilation (see [`crate::machine`]): this module
//! only validates *shape*; target resolution and structural rules happen when the
//! [`Machine`](crate::machine::Machine) is built.

use serde_json::Value;

use crate::error::Result;

mod parse;

pub use parse::ParseOptions;

/// A parsed machine definition.
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// The root state node (its `key` is the machine id).
    pub root: StateConfig,
    /// Initial context value (`null` when absent).
    pub context: Value,
    /// Optional chart version label (`version` key), coerced to a string.
    pub version: Option<String>,
    /// Runtime policies declared on the root.
    pub policies: Policies,
    /// Non-fatal findings collected while parsing (unknown keys and the like).
    pub warnings: Vec<ConfigWarning>,
    /// The XState JSON document this configuration stands for: the parsed input verbatim, or the
    /// equivalent document for programmatically built configurations (`Machine::to_json`).
    pub source: Value,
}

/// A non-fatal parse finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// Location inside the document, e.g. `states.a.on.GO`.
    pub path: String,
    /// What was found.
    pub message: String,
}

/// Root-level runtime policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policies {
    /// `strict`: an event no state declares is an error instead of being ignored.
    pub strict: bool,
    /// `strictTargets`: refuse the deprecated sibling fallback for `.child` targets.
    pub strict_targets: bool,
    /// `strictConfig`: unknown keys are errors instead of warnings.
    pub strict_config: bool,
    /// `maxIterations`: eventless / self-generated event budget per macrostep.
    pub max_iterations: u32,
    /// `actionErrorPolicy`.
    pub action_error_policy: ActionErrorPolicy,
    /// `guardErrorPolicy`.
    pub guard_error_policy: GuardErrorPolicy,
    /// `onUnhandled`.
    pub on_unhandled: UnhandledPolicy,
}

impl Default for Policies {
    fn default() -> Self {
        Self {
            strict: false,
            strict_targets: true,
            strict_config: false,
            max_iterations: crate::DEFAULT_MAX_ITERATIONS,
            action_error_policy: ActionErrorPolicy::Fail,
            guard_error_policy: GuardErrorPolicy::False,
            on_unhandled: UnhandledPolicy::Ignore,
        }
    }
}

/// What happens when an action implementation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionErrorPolicy {
    /// Log, skip the rest of that action list, keep going (the Python default before 1.0).
    Continue,
    /// Restore configuration and context as they were before the step.
    Rollback,
    /// Fail the `send` with [`Error::TransitionFailed`](crate::Error::TransitionFailed) and move the machine to `error`
    /// (XState behaviour; the default).
    Fail,
}

/// What a guard evaluates to when its implementation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardErrorPolicy {
    /// Treat as `false` (SCXML §5.9).
    False,
    /// Treat as `true`.
    True,
    /// Propagate the error.
    Raise,
}

/// What happens to an event no active state handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnhandledPolicy {
    /// Drop it (XState behaviour).
    Ignore,
    /// Buffer it and replay after the configuration changes.
    Defer,
    /// Move the machine to the `error` status.
    Error,
}

/// Kind of a state node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    /// Leaf state.
    Atomic,
    /// Has children and exactly one active child.
    Compound,
    /// Has children that are all active (regions).
    Parallel,
    /// Terminal state of its parent.
    Final,
    /// History pseudo-state.
    History {
        /// `deep` restores leaves, shallow restores direct children.
        deep: bool,
        /// Default target when no history was recorded.
        target: Option<String>,
    },
}

/// One state node as written in the document.
#[derive(Debug, Clone)]
pub struct StateConfig {
    /// Key under the parent's `states` (or the machine id for the root).
    pub key: String,
    /// Custom `id` for `#id` targets.
    pub custom_id: Option<String>,
    /// Node kind.
    pub kind: StateKind,
    /// `initial` child key (compound states; inferred when there is exactly one child).
    pub initial: Option<String>,
    /// Children in document order.
    pub states: Vec<StateConfig>,
    /// `on` handlers in document order; each descriptor maps to its candidate list.
    pub on: Vec<(String, Vec<TransitionConfig>)>,
    /// Eventless transitions (`always`, or the legacy `on: {"": ...}`).
    pub always: Vec<TransitionConfig>,
    /// Delayed transitions keyed by the raw `after` key (number or delay name).
    pub after: Vec<(String, Vec<TransitionConfig>)>,
    /// Entry actions.
    pub entry: Vec<ActionConfig>,
    /// Exit actions.
    pub exit: Vec<ActionConfig>,
    /// Invoked services.
    pub invoke: Vec<InvokeConfig>,
    /// `onDone` of a compound/parallel state.
    pub on_done: Vec<TransitionConfig>,
    /// `output` of a final state or of the machine.
    pub output: Option<Value>,
    /// Free-form metadata.
    pub meta: Option<Value>,
    /// Human description.
    pub description: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
}

/// A transition as written in the document.
#[derive(Debug, Clone, Default)]
pub struct TransitionConfig {
    /// Raw target strings (empty for targetless transitions).
    pub targets: Vec<String>,
    /// Actions in order.
    pub actions: Vec<ActionConfig>,
    /// Guard, if any.
    pub guard: Option<GuardConfig>,
    /// `reenter` (v5) / `internal: false` (v4).
    pub reenter: bool,
    /// `"E": null` — consume the event and block ancestors.
    pub forbidden: bool,
    /// Free-form metadata.
    pub meta: Option<Value>,
    /// Human description.
    pub description: Option<String>,
}

/// A guard expression.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardConfig {
    /// Named guard (user-defined or the built-in `stateIn`).
    Named {
        /// Guard name.
        name: String,
        /// `params` (or `null`).
        params: Value,
    },
    /// Logical negation.
    Not(Box<GuardConfig>),
    /// All operands must hold.
    And(Vec<GuardConfig>),
    /// At least one operand must hold.
    Or(Vec<GuardConfig>),
}

/// An action reference.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionConfig {
    /// Action name (user-defined, or a built-in in any accepted spelling).
    pub name: String,
    /// `params` (or `null`).
    pub params: Value,
}

/// An `invoke` entry.
#[derive(Debug, Clone)]
pub struct InvokeConfig {
    /// Invoke id (defaults to the owning state's id at compile time).
    pub id: Option<String>,
    /// Service / actor logic name.
    pub src: String,
    /// Static input value (`null` when absent).
    pub input: Value,
    /// `systemId` registration name.
    pub system_id: Option<String>,
    /// Transitions taken on completion.
    pub on_done: Vec<TransitionConfig>,
    /// Transitions taken on failure.
    pub on_error: Vec<TransitionConfig>,
}

impl MachineConfig {
    /// Parse an XState JSON document with default options.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`](crate::Error::InvalidConfig) for shape problems.
    pub fn from_value(value: &Value) -> Result<Self> {
        Self::from_value_with(value, &ParseOptions::default())
    }

    /// Parse with explicit options.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`](crate::Error::InvalidConfig) for shape problems, including unknown keys when
    /// `strictConfig` is in force.
    pub fn from_value_with(value: &Value, options: &ParseOptions) -> Result<Self> {
        parse::parse_machine(value, options, None)
    }

    /// Parse, treating every name in `user_actions` as the user's own action even when it is
    /// spelled like a built-in (`log`, `cancel`, ...): its `params` are not held to the
    /// built-in's signature, since user logic wins over built-ins.
    pub(crate) fn from_value_for(
        value: &Value,
        options: &ParseOptions,
        user_actions: &std::collections::HashSet<String>,
    ) -> Result<Self> {
        parse::parse_machine(value, options, Some(user_actions))
    }

    /// Parse from JSON text.
    ///
    /// # Errors
    /// Returns [`Error::Json`](crate::Error::Json) for malformed JSON and [`Error::InvalidConfig`](crate::Error::InvalidConfig) for shape
    /// problems.
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)?;
        Self::from_value(&value)
    }
}

impl StateConfig {
    fn new(key: String) -> Self {
        Self {
            key,
            custom_id: None,
            kind: StateKind::Atomic,
            initial: None,
            states: Vec::new(),
            on: Vec::new(),
            always: Vec::new(),
            after: Vec::new(),
            entry: Vec::new(),
            exit: Vec::new(),
            invoke: Vec::new(),
            on_done: Vec::new(),
            output: None,
            meta: None,
            description: None,
            tags: Vec::new(),
        }
    }
}

/// Parse a guard value (string or object form). Used by the `choose` built-in compiler.
pub(crate) fn parse_guard_value(
    value: &Value,
    path: &str,
    strict: bool,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<GuardConfig> {
    parse::parse_guard_at(value, path, strict, warnings)
}

/// Parse an action list value. Used by the `choose` built-in compiler.
pub(crate) fn parse_actions_value(
    value: Option<&Value>,
    path: &str,
    strict: bool,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<Vec<ActionConfig>> {
    parse::parse_actions_at(value, path, strict, warnings)
}
