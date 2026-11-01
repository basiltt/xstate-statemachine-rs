//! Fluent builder API for defining machines in Rust instead of JSON.
//!
//! [`MachineBuilder`] assembles the same [`MachineConfig`] tree the JSON parser produces, so a
//! machine built here and the equivalent JSON document are interchangeable: they compile to
//! machines with the same [`structure_hash`](crate::Machine::structure_hash), and either one
//! can be paired with the same [`MachineLogic`](crate::MachineLogic).
//!
//! The vocabulary is XState's: `initial`, `on`, `always`, `after`, `invoke`, `onDone`,
//! `entry` / `exit`, `meta`, `tags`; states are atomic, compound, parallel, final or history.
//!
//! * Nested states are described with a closure over a [`StateBuilder`].
//! * A handler is a bare target (`.on("GO", "b")`), a [`Transition`], or a list of them (the
//!   first transition whose guard holds wins).
//! * [`Action`], [`Guard`], [`Invoke`], [`History`] and [`DelayKey`] cover the rest.
//!
//! Mistakes that the type system cannot rule out (a compound state without `initial`, two
//! handlers for one event, a non-object `meta`) are reported by
//! [`MachineBuilder::build_config`] / [`MachineBuilder::build`] as
//! [`Error::InvalidConfig`] with a JSON-style path such as `states.form.initial`.

use serde::Serialize;
use serde_json::Value;

use crate::config::{
    ActionConfig, ActionErrorPolicy, GuardConfig, GuardErrorPolicy, InvokeConfig, MachineConfig,
    Policies, StateConfig, StateKind, TransitionConfig, UnhandledPolicy,
};
use crate::error::{Error, Result};
use crate::machine::Machine;

mod parts;
mod render;

pub use parts::{Action, Branch, DelayKey, Guard, History, IntoTransitions, Invoke, Transition};

/// A mistake noticed while chaining, reported when the builder is finished.
#[derive(Debug, Clone)]
struct Problem {
    /// Path relative to the owning state (`on.GO`, `meta`).
    what: String,
    message: String,
}

/// Mutable state of one node under construction.
#[derive(Debug, Clone)]
struct Node {
    key: String,
    custom_id: Option<String>,
    /// Explicit kind; inferred from the children when `None`.
    kind: Option<StateKind>,
    initial: Option<String>,
    children: Vec<Node>,
    on: Vec<(String, Vec<TransitionConfig>)>,
    always: Vec<TransitionConfig>,
    after: Vec<(String, Vec<TransitionConfig>)>,
    entry: Vec<ActionConfig>,
    exit: Vec<ActionConfig>,
    invoke: Vec<InvokeConfig>,
    on_done: Vec<TransitionConfig>,
    output: Option<Value>,
    meta: Option<Value>,
    description: Option<String>,
    tags: Vec<String>,
    problems: Vec<Problem>,
}

impl Node {
    fn new(key: impl Into<String>, kind: Option<StateKind>) -> Self {
        Self {
            key: key.into(),
            custom_id: None,
            kind,
            initial: None,
            children: Vec::new(),
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
            problems: Vec::new(),
        }
    }

    fn problem(&mut self, what: impl Into<String>, message: impl Into<String>) {
        self.problems.push(Problem { what: what.into(), message: message.into() });
    }

    fn add_child(&mut self, child: Node) {
        self.children.push(child);
    }

    fn push_on(&mut self, event: String, transitions: Vec<TransitionConfig>) {
        // `""` is the legacy spelling of an eventless transition; the JSON parser folds it into
        // `always`, and so do we.
        if event.is_empty() {
            self.always.extend(transitions);
        } else if self.on.iter().any(|(existing, _)| *existing == event) {
            self.problem(
                format!("on.{event}"),
                "event already has a handler; pass a list of transitions instead",
            );
        } else {
            self.on.push((event, transitions));
        }
    }

    fn push_after(&mut self, delay: &DelayKey, transitions: Vec<TransitionConfig>) {
        let key = delay.key();
        if self.after.iter().any(|(existing, _)| *existing == key) {
            self.problem(format!("after.{key}"), "delay already has a handler");
        } else {
            self.after.push((key, transitions));
        }
    }

    fn set_json<T: Serialize>(&mut self, what: &str, value: &T) -> Option<Value> {
        match serde_json::to_value(value) {
            Ok(v) => Some(v),
            Err(err) => {
                self.problem(what, format!("value is not serialisable: {err}"));
                None
            }
        }
    }

    fn set_meta<T: Serialize>(&mut self, meta: &T) {
        match self.set_json("meta", meta) {
            Some(v @ Value::Object(_)) => self.meta = Some(v),
            Some(_) => self.problem("meta", "`meta` must be an object"),
            None => {}
        }
    }

    fn set_output<T: Serialize>(&mut self, output: &T) {
        self.output = self.set_json("output", output);
    }
}

/// Generates the state-level methods shared by [`MachineBuilder`] (which configures the root)
/// and [`StateBuilder`] (which configures a nested state). The expanding type must provide
/// `fn node_mut(&mut self) -> &mut Node`.
macro_rules! state_methods {
    () => {
        /// Set the `initial` child of a compound state. Optional when there is exactly one
        /// (non-history) child, in which case it is inferred, as the JSON parser does.
        #[must_use]
        pub fn initial(mut self, key: impl Into<String>) -> Self {
            self.node_mut().initial = Some(key.into());
            self
        }

        /// Mark this state as a parallel state: all of its children are active at once.
        #[must_use]
        pub fn parallel(mut self) -> Self {
            self.node_mut().kind = Some(StateKind::Parallel);
            self
        }

        /// Append an entry action (a name, or an [`Action`] for built-ins and params).
        #[must_use]
        pub fn entry(mut self, action: impl Into<Action>) -> Self {
            self.node_mut().entry.push(action.into().0);
            self
        }

        /// Append an exit action.
        #[must_use]
        pub fn exit(mut self, action: impl Into<Action>) -> Self {
            self.node_mut().exit.push(action.into().0);
            self
        }

        /// Handle `event` with a bare target, a [`Transition`], or a list of transitions
        /// (first match wins). Descriptors may use XState wildcards: `"*"`, `"user.*"`.
        ///
        /// Each event may be handled once per state; use a list for alternatives. The empty
        /// event `""` is folded into [`always`](Self::always).
        #[must_use]
        pub fn on(mut self, event: impl Into<String>, handler: impl IntoTransitions) -> Self {
            self.node_mut().push_on(event.into(), handler.into_transitions());
            self
        }

        /// Forbid `event` here: it is consumed and nothing happens, so handlers on ancestor
        /// states do not see it (`"E": null` in JSON).
        #[must_use]
        pub fn forbid(self, event: impl Into<String>) -> Self {
            self.on(event, Transition::forbidden())
        }

        /// Add eventless (`always`) transitions, evaluated after every step until none fires.
        #[must_use]
        pub fn always(mut self, handler: impl IntoTransitions) -> Self {
            self.node_mut().always.extend(handler.into_transitions());
            self
        }

        /// Add a delayed transition: after a fixed number of milliseconds (`500`, a
        /// [`Duration`](std::time::Duration)) or a named delay (`"BACKOFF"`) in this state.
        #[must_use]
        pub fn after(mut self, delay: impl Into<DelayKey>, handler: impl IntoTransitions) -> Self {
            self.node_mut().push_after(&delay.into(), handler.into_transitions());
            self
        }

        /// Invoke a service while this state is active.
        #[must_use]
        pub fn invoke(mut self, invoke: Invoke) -> Self {
            self.node_mut().invoke.push(invoke.0);
            self
        }

        /// Transition taken when this compound/parallel state completes (`onDone`).
        #[must_use]
        pub fn on_done(mut self, handler: impl IntoTransitions) -> Self {
            self.node_mut().on_done = handler.into_transitions();
            self
        }

        /// `output` of a final state, or of the machine when set on the root. Any
        /// serialisable value.
        #[must_use]
        pub fn output<T: Serialize>(mut self, output: T) -> Self {
            self.node_mut().set_output(&output);
            self
        }

        /// Free-form metadata; must serialise to a JSON object.
        #[must_use]
        pub fn meta<T: Serialize>(mut self, meta: T) -> Self {
            self.node_mut().set_meta(&meta);
            self
        }

        /// Human-readable description.
        #[must_use]
        pub fn description(mut self, description: impl Into<String>) -> Self {
            self.node_mut().description = Some(description.into());
            self
        }

        /// Add one tag.
        #[must_use]
        pub fn tag(mut self, tag: impl Into<String>) -> Self {
            self.node_mut().tags.push(tag.into());
            self
        }

        /// Add several tags.
        #[must_use]
        pub fn tags(mut self, tags: impl IntoIterator<Item: Into<String>>) -> Self {
            self.node_mut().tags.extend(tags.into_iter().map(Into::into));
            self
        }

        /// Add a child state configured by `build`. The state is compound when it ends up with
        /// children and atomic otherwise.
        #[must_use]
        pub fn state(
            mut self,
            key: impl Into<String>,
            build: impl FnOnce(StateBuilder) -> StateBuilder,
        ) -> Self {
            let child = build(StateBuilder(Node::new(key, None)));
            self.node_mut().add_child(child.0);
            self
        }

        /// Add an atomic child state with no configuration of its own.
        #[must_use]
        pub fn atomic(self, key: impl Into<String>) -> Self {
            self.state(key, |s| s)
        }

        /// Add a parallel child state configured by `build`.
        #[must_use]
        pub fn parallel_state(
            mut self,
            key: impl Into<String>,
            build: impl FnOnce(StateBuilder) -> StateBuilder,
        ) -> Self {
            let child = build(StateBuilder(Node::new(key, Some(StateKind::Parallel))));
            self.node_mut().add_child(child.0);
            self
        }

        /// Add a plain final child state.
        #[must_use]
        pub fn final_state(self, key: impl Into<String>) -> Self {
            self.final_state_with(key, |s| s)
        }

        /// Add a final child state configured by `build` (typically with an `output`).
        #[must_use]
        pub fn final_state_with(
            mut self,
            key: impl Into<String>,
            build: impl FnOnce(StateBuilder) -> StateBuilder,
        ) -> Self {
            let child = build(StateBuilder(Node::new(key, Some(StateKind::Final))));
            self.node_mut().add_child(child.0);
            self
        }

        /// Add a history pseudo-state child.
        #[must_use]
        pub fn history(mut self, key: impl Into<String>, history: History) -> Self {
            let kind = StateKind::History { deep: history.deep, target: history.target };
            self.node_mut().add_child(Node::new(key, Some(kind)));
            self
        }
    };
}

/// Builder for one nested state, handed to the closure passed to `.state(..)`.
///
/// Every method is also available on [`MachineBuilder`], which configures the root state.
/// See the [module documentation](self) for the vocabulary.
#[derive(Debug, Clone)]
pub struct StateBuilder(Node);

impl StateBuilder {
    fn node_mut(&mut self) -> &mut Node {
        &mut self.0
    }

    /// Custom `id`, making the state addressable as `#id` from any transition target.
    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.0.custom_id = Some(id.into());
        self
    }

    state_methods!();
}

/// Fluent builder for a whole machine.
///
/// Produces a [`MachineConfig`] with [`build_config`](Self::build_config) or a compiled
/// [`Machine`] with [`build`](Self::build). The builder is owned and every method returns it,
/// so a machine is one expression; it is `Clone` when a template needs to be reused.
///
/// # Example
///
/// ```
/// use serde_json::json;
/// use xstate_core::builder::{Action, Guard, History, Invoke, MachineBuilder, Transition};
/// use xstate_core::{MachineLogic, SyncInterpreter};
///
/// let machine = MachineBuilder::new("checkout")
///     .version("1.0")
///     .context(json!({ "attempts": 0 }))
///     .strict(true)
///     .initial("cart")
///     // A compound state with nested states, history and a global escape hatch.
///     .state("cart", |cart| {
///         cart.initial("browsing")
///             .entry("logCartOpened")
///             .state("browsing", |s| s.on("CHECKOUT", "payment"))
///             .state("editing", |s| s.on("DONE", "browsing"))
///             .history("last", History::shallow().target("browsing"))
///             .on("EDIT", ".editing")
///     })
///     .state("payment", |pay| {
///         pay.on("PAY", [
///                 Transition::to("confirmed").guard("hasFunds").action("charge"),
///                 Transition::to("declined"),
///             ])
///             .after(30_000_u64, "cart")
///             .on("CANCEL", Transition::to("cart.last").reenter())
///             .forbid("EDIT")
///     })
///     .state("confirmed", |s| {
///         s.entry(Action::assign(json!({ "attempts": 1 })))
///             .always(Transition::to("done").guard(Guard::not("needsReview")))
///     })
///     .state("declined", |s| s.on("RETRY", "payment"))
///     .state("review", |s| s.invoke(
///         Invoke::new("manualReview").on_done("done").on_error("declined"),
///     ))
///     .final_state_with("done", |s| s.output(json!({ "ok": true })))
///     .build()?;
///
/// let logic = MachineLogic::dynamic()
///     .action("logCartOpened", |_| Ok(()))
///     .action("charge", |_| Ok(()))
///     .guard("hasFunds", |_, _, _| Ok(true))
///     .guard("needsReview", |_, _, _| Ok(false))
///     .service("manualReview", |_, _| Ok(json!("approved")));
/// let mut interp = SyncInterpreter::new(machine, logic);
/// interp.start()?;
/// interp.send("CHECKOUT")?;
/// interp.send("PAY")?;
/// assert_eq!(interp.state_ids(), ["checkout.done"]);
/// # Ok::<(), xstate_core::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct MachineBuilder {
    root: Node,
    context: Value,
    version: Option<String>,
    policies: Policies,
}

impl MachineBuilder {
    /// Start a machine with the given id (the root state's key; state ids are
    /// `<id>.<state>`).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            root: Node::new(id, None),
            context: Value::Null,
            version: None,
            policies: Policies::default(),
        }
    }

    fn node_mut(&mut self) -> &mut Node {
        &mut self.root
    }

    /// Initial context: a [`serde_json::Value`] or any [`Serialize`] type.
    #[must_use]
    pub fn context<T: Serialize>(mut self, context: T) -> Self {
        if let Some(value) = self.root.set_json("context", &context) {
            self.context = value;
        }
        self
    }

    /// Chart version label (`version`).
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Replace every runtime policy at once.
    #[must_use]
    pub fn policies(mut self, policies: Policies) -> Self {
        self.policies = policies;
        self
    }

    /// `strict`: sending an event no state declares is an error instead of being ignored.
    #[must_use]
    pub fn strict(mut self, strict: bool) -> Self {
        self.policies.strict = strict;
        self
    }

    /// `strictTargets`: refuse the deprecated sibling fallback for `.child` targets.
    #[must_use]
    pub fn strict_targets(mut self, strict_targets: bool) -> Self {
        self.policies.strict_targets = strict_targets;
        self
    }

    /// `strictConfig`: unknown keys are errors instead of warnings (kept for parity with
    /// the JSON form; a builder has no unknown keys).
    #[must_use]
    pub fn strict_config(mut self, strict_config: bool) -> Self {
        self.policies.strict_config = strict_config;
        self
    }

    /// `maxIterations`: eventless / self-generated event budget per macrostep (at least 1).
    #[must_use]
    pub fn max_iterations(mut self, max_iterations: u32) -> Self {
        self.policies.max_iterations = max_iterations;
        self
    }

    /// `actionErrorPolicy`.
    #[must_use]
    pub fn action_error_policy(mut self, policy: ActionErrorPolicy) -> Self {
        self.policies.action_error_policy = policy;
        self
    }

    /// `guardErrorPolicy`.
    #[must_use]
    pub fn guard_error_policy(mut self, policy: GuardErrorPolicy) -> Self {
        self.policies.guard_error_policy = policy;
        self
    }

    /// `onUnhandled`.
    #[must_use]
    pub fn on_unhandled(mut self, policy: UnhandledPolicy) -> Self {
        self.policies.on_unhandled = policy;
        self
    }

    state_methods!();

    /// Produce the configuration tree, exactly as [`MachineConfig::from_value`] would for the
    /// equivalent JSON document (without warnings).
    ///
    /// # Errors
    /// [`Error::InvalidConfig`] for an empty machine id, empty state keys or ids, duplicate
    /// child keys, a compound state without `initial` (and not exactly one child to infer it
    /// from), a second handler for one event or delay, a non-object `meta`, an unserialisable
    /// value, an empty action / guard / service name, or `max_iterations(0)`.
    pub fn build_config(self) -> Result<MachineConfig> {
        if self.root.key.is_empty() {
            return Err(Error::config("id", "machine `id` must be a non-empty string"));
        }
        if self.policies.max_iterations == 0 {
            return Err(Error::config(
                "maxIterations",
                "`maxIterations` must be a positive integer",
            ));
        }
        let root = finish(self.root, "")?;
        let source =
            render::machine_document(&root, &self.context, self.version.as_deref(), &self.policies);
        Ok(MachineConfig {
            root,
            context: self.context,
            version: self.version,
            policies: self.policies,
            warnings: Vec::new(),
            source,
        })
    }

    /// Build and compile the machine.
    ///
    /// # Errors
    /// Everything [`build_config`](Self::build_config) reports, plus the structural errors of
    /// [`Machine::from_config`]: unresolved targets ([`Error::StateNotFound`]), an `initial`
    /// naming an unknown child, targets of the root, and so on.
    pub fn build(self) -> Result<Machine> {
        Machine::from_config(self.build_config()?)
    }
}

// ---------------------------------------------------------------------------------------------
// Finishing: Node tree -> StateConfig tree
// ---------------------------------------------------------------------------------------------

fn join(path: &str, segment: &str) -> String {
    if path.is_empty() { segment.to_owned() } else { format!("{path}.{segment}") }
}

fn finish(mut node: Node, path: &str) -> Result<StateConfig> {
    if let Some(problem) = node.problems.first() {
        return Err(Error::config(join(path, &problem.what), problem.message.clone()));
    }
    if node.custom_id.as_deref() == Some("") {
        return Err(Error::config(join(path, "id"), "state `id` must be a non-empty string"));
    }

    let states_path = join(path, "states");
    let mut states: Vec<StateConfig> = Vec::with_capacity(node.children.len());
    for child in std::mem::take(&mut node.children) {
        let child_path = join(&states_path, &child.key);
        if child.key.is_empty() {
            return Err(Error::config(child_path, "state keys must be non-empty"));
        }
        if states.iter().any(|existing| existing.key == child.key) {
            return Err(Error::config(child_path, "duplicate state key"));
        }
        states.push(finish(child, &child_path)?);
    }

    let kind = node.kind.take().unwrap_or(if states.is_empty() {
        StateKind::Atomic
    } else {
        StateKind::Compound
    });
    let initial = resolve_initial(node.initial.take(), &kind, &states, path)?;

    let state = StateConfig {
        key: node.key,
        custom_id: node.custom_id,
        kind,
        initial,
        states,
        on: node.on,
        always: node.always,
        after: node.after,
        entry: node.entry,
        exit: node.exit,
        invoke: node.invoke,
        on_done: node.on_done,
        output: node.output,
        meta: node.meta,
        description: node.description,
        tags: node.tags,
    };
    validate_handlers(&state, path)?;
    Ok(state)
}

/// Same rule as the JSON parser: an explicit `initial` wins; a compound state with exactly
/// one non-history child infers it; anything else is an error.
fn resolve_initial(
    explicit: Option<String>,
    kind: &StateKind,
    states: &[StateConfig],
    path: &str,
) -> Result<Option<String>> {
    if *kind != StateKind::Compound || explicit.is_some() {
        return Ok(explicit);
    }
    let mut candidates = states.iter().filter(|s| !matches!(s.kind, StateKind::History { .. }));
    match (candidates.next(), candidates.next()) {
        (Some(only), None) => Ok(Some(only.key.clone())),
        _ => Err(Error::config(join(path, "initial"), "compound state needs an `initial` child")),
    }
}

// ---------------------------------------------------------------------------------------------
// Validation of names (the JSON parser rejects the same things)
// ---------------------------------------------------------------------------------------------

fn validate_handlers(state: &StateConfig, path: &str) -> Result<()> {
    check_actions(&state.entry, &join(path, "entry"))?;
    check_actions(&state.exit, &join(path, "exit"))?;
    for (event, transitions) in &state.on {
        check_transitions(transitions, &join(&join(path, "on"), event))?;
    }
    check_transitions(&state.always, &join(path, "always"))?;
    for (delay, transitions) in &state.after {
        check_transitions(transitions, &join(&join(path, "after"), delay))?;
    }
    check_transitions(&state.on_done, &join(path, "onDone"))?;
    for (i, invoke) in state.invoke.iter().enumerate() {
        let invoke_path = format!("{}[{i}]", join(path, "invoke"));
        if invoke.src.is_empty() {
            return Err(Error::config(
                join(&invoke_path, "src"),
                "`src` must be a non-empty string",
            ));
        }
        check_transitions(&invoke.on_done, &join(&invoke_path, "onDone"))?;
        check_transitions(&invoke.on_error, &join(&invoke_path, "onError"))?;
    }
    Ok(())
}

fn check_transitions(transitions: &[TransitionConfig], path: &str) -> Result<()> {
    for (i, transition) in transitions.iter().enumerate() {
        let path = format!("{path}[{i}]");
        check_actions(&transition.actions, &join(&path, "actions"))?;
        if let Some(guard) = &transition.guard {
            check_guard(guard, &join(&path, "guard"))?;
        }
    }
    Ok(())
}

fn check_actions(actions: &[ActionConfig], path: &str) -> Result<()> {
    match actions.iter().position(|a| a.name.is_empty()) {
        Some(i) => Err(Error::config(format!("{path}[{i}]"), "action name must be non-empty")),
        None => Ok(()),
    }
}

fn check_guard(guard: &GuardConfig, path: &str) -> Result<()> {
    match guard {
        GuardConfig::Named { name, .. } if name.is_empty() => {
            Err(Error::config(path, "guard name must be non-empty"))
        }
        GuardConfig::Named { .. } => Ok(()),
        GuardConfig::Not(inner) => check_guard(inner, path),
        GuardConfig::And(list) | GuardConfig::Or(list) => {
            list.iter().try_for_each(|g| check_guard(g, path))
        }
    }
}
