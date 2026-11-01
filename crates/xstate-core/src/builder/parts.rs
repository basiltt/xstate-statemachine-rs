//! Value types the fluent builders are made of: [`Action`], [`Guard`], [`Transition`],
//! [`Invoke`], [`History`] and [`DelayKey`].
//!
//! Each is a thin, typed wrapper over the corresponding [`crate::config`] node, so what the
//! builder produces is exactly what the JSON parser would have produced.

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::render;
use crate::config::{ActionConfig, GuardConfig, InvokeConfig, TransitionConfig};

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

/// An action reference: a user-defined action name or an XState built-in, plus `params`.
///
/// Plain strings convert into actions, so `entry("log")` and `entry(Action::named("log"))`
/// are the same thing.
///
/// ```
/// use xstate_core::builder::Action;
///
/// let user = Action::named("notify").params(serde_json::json!({ "channel": "ops" }));
/// let raise = Action::raise("RETRY").delay(250_u64).id("retry-timer");
/// let assign = Action::assign(serde_json::json!({ "attempts": 0 }));
/// # let _ = (user, raise, assign);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Action(pub(super) ActionConfig);

impl Action {
    /// A named action (user-defined, or a built-in spelled by name) without parameters.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self(ActionConfig { name: name.into(), params: Value::Null })
    }

    /// Replace the action's `params` wholesale.
    #[must_use]
    pub fn params(mut self, params: impl Into<Value>) -> Self {
        self.0.params = params.into();
        self
    }

    /// Set one key of the action's `params` object (creating it when absent).
    #[must_use]
    pub fn param(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let mut map = match std::mem::take(&mut self.0.params) {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        map.insert(key.into(), value.into());
        self.0.params = Value::Object(map);
        self
    }

    /// Set `params.delay` (for `raise` / `sendTo`): milliseconds or a named delay.
    #[must_use]
    pub fn delay(self, delay: impl Into<DelayKey>) -> Self {
        self.param("delay", delay.into().to_value())
    }

    /// Set `params.id` (for `raise` / `sendTo` / `spawnChild` / `stopChild`).
    #[must_use]
    pub fn id(self, id: impl Into<String>) -> Self {
        self.param("id", id.into())
    }

    /// Built-in `assign` with a static patch merged into the context.
    #[must_use]
    pub fn assign(assignment: impl Into<Value>) -> Self {
        Self::named("assign").param("assignment", assignment)
    }

    /// Built-in `raise` of an event with the given type.
    #[must_use]
    pub fn raise(event_type: impl Into<String>) -> Self {
        Self::named("raise").param("event", json!({ "type": event_type.into() }))
    }

    /// Built-in `raise` of a full event object (`{"type": ..., ...payload}`).
    #[must_use]
    pub fn raise_event(event: impl Into<Value>) -> Self {
        Self::named("raise").param("event", event)
    }

    /// Built-in `sendTo` an actor (`"parent"`, `"self"`, a child id or a system id).
    #[must_use]
    pub fn send_to(to: impl Into<String>, event_type: impl Into<String>) -> Self {
        Self::named("sendTo")
            .param("to", to.into())
            .param("event", json!({ "type": event_type.into() }))
    }

    /// Built-in `sendParent`.
    #[must_use]
    pub fn send_parent(event_type: impl Into<String>) -> Self {
        Self::named("sendParent").param("event", json!({ "type": event_type.into() }))
    }

    /// Built-in `forwardTo` an actor.
    #[must_use]
    pub fn forward_to(to: impl Into<String>) -> Self {
        Self::named("forwardTo").param("to", to.into())
    }

    /// Built-in `escalate` with an error value.
    #[must_use]
    pub fn escalate(error: impl Into<Value>) -> Self {
        Self::named("escalate").param("error", error)
    }

    /// Built-in `log` of a static expression.
    #[must_use]
    pub fn log(expr: impl Into<Value>) -> Self {
        Self::named("log").param("expr", expr)
    }

    /// Built-in `cancel` of a delayed send by its `id`.
    #[must_use]
    pub fn cancel(send_id: impl Into<String>) -> Self {
        Self::named("cancel").param("sendId", send_id.into())
    }

    /// Built-in `stopChild` by child id.
    #[must_use]
    pub fn stop_child(id: impl Into<String>) -> Self {
        Self::named("stopChild").param("id", id.into())
    }

    /// Built-in `spawnChild` of the named logic.
    #[must_use]
    pub fn spawn_child(src: impl Into<String>) -> Self {
        Self::named("spawnChild").param("src", src.into())
    }

    /// Built-in `emit` of an event with the given type.
    #[must_use]
    pub fn emit(event_type: impl Into<String>) -> Self {
        Self::named("emit").param("event", json!({ "type": event_type.into() }))
    }

    /// Built-in `choose`: the actions of the first branch whose guard holds.
    ///
    /// ```
    /// use xstate_core::builder::{Action, Branch, Guard};
    ///
    /// let action = Action::choose([
    ///     Branch::when(Guard::state_in("#m.mode.night"), ["atNight"]),
    ///     Branch::otherwise(["otherwise"]),
    /// ]);
    /// # let _ = action;
    /// ```
    #[must_use]
    pub fn choose(branches: impl IntoIterator<Item = Branch>) -> Self {
        let branches: Vec<Value> = branches.into_iter().map(Branch::into_value).collect();
        Self::named("choose").param("branches", Value::Array(branches))
    }

    /// The JSON spelling: a bare name without params, `{"type", "params"}` otherwise.
    fn to_value(&self) -> Value {
        render::action_value(&self.0)
    }
}

impl From<&str> for Action {
    fn from(name: &str) -> Self {
        Self::named(name)
    }
}

impl From<String> for Action {
    fn from(name: String) -> Self {
        Self::named(name)
    }
}

/// One branch of a [`Action::choose`]: an optional guard and the actions it selects.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    guard: Option<Guard>,
    actions: Vec<Action>,
}

impl Branch {
    /// A branch taken when `guard` holds.
    #[must_use]
    pub fn when(guard: impl Into<Guard>, actions: impl IntoIterator<Item: Into<Action>>) -> Self {
        Self { guard: Some(guard.into()), actions: actions.into_iter().map(Into::into).collect() }
    }

    /// The unguarded fallback branch.
    #[must_use]
    pub fn otherwise(actions: impl IntoIterator<Item: Into<Action>>) -> Self {
        Self { guard: None, actions: actions.into_iter().map(Into::into).collect() }
    }

    fn into_value(self) -> Value {
        let mut map = Map::new();
        if let Some(guard) = &self.guard {
            map.insert("guard".to_owned(), guard.to_value());
        }
        let actions = self.actions.iter().map(Action::to_value).collect();
        map.insert("actions".to_owned(), render::one_or_many(actions));
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------------

/// A guard expression: a named guard, `stateIn`, or `not` / `and` / `or` over other guards.
///
/// Strings convert into guards. As in the JSON form, a leading `!` negates:
/// `"!isBanned"` is `Guard::not("isBanned")`.
///
/// ```
/// use xstate_core::builder::Guard;
///
/// let guard = Guard::and([Guard::named("isAdmin"), Guard::not("isBanned")]);
/// let in_state = Guard::state_in("#app.mode.night");
/// # let _ = (guard, in_state);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Guard(pub(super) GuardConfig);

impl Guard {
    /// A guard implemented in the logic registry under `name`.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self(GuardConfig::Named { name: name.into(), params: Value::Null })
    }

    /// A named guard with static `params`.
    #[must_use]
    pub fn with_params(name: impl Into<String>, params: impl Into<Value>) -> Self {
        Self(GuardConfig::Named { name: name.into(), params: params.into() })
    }

    /// Logical negation.
    #[must_use]
    pub fn not(inner: impl Into<Guard>) -> Self {
        Self(GuardConfig::Not(Box::new(inner.into().0)))
    }

    /// All operands must hold.
    #[must_use]
    pub fn and(operands: impl IntoIterator<Item: Into<Guard>>) -> Self {
        Self(GuardConfig::And(operands.into_iter().map(|g| g.into().0).collect()))
    }

    /// At least one operand must hold.
    #[must_use]
    pub fn or(operands: impl IntoIterator<Item: Into<Guard>>) -> Self {
        Self(GuardConfig::Or(operands.into_iter().map(|g| g.into().0).collect()))
    }

    /// Built-in `stateIn`: holds while the referenced state (`#id`, a full id or a path) is
    /// active.
    #[must_use]
    pub fn state_in(state: impl Into<String>) -> Self {
        Self::with_params("stateIn", json!({ "stateValue": state.into() }))
    }

    /// The JSON spelling (see the parser's guard grammar).
    fn to_value(&self) -> Value {
        render::guard_value(&self.0)
    }
}

impl From<&str> for Guard {
    fn from(name: &str) -> Self {
        match name.strip_prefix('!') {
            Some(inner) => Self::not(Self::named(inner)),
            None => Self::named(name),
        }
    }
}

impl From<String> for Guard {
    fn from(name: String) -> Self {
        Self::from(name.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------------------------

/// One transition: target(s), guard, actions and `reenter`.
///
/// Wherever a handler is expected a bare target string works too (`.on("GO", "b")`); use
/// `Transition` when the transition needs more than a target.
///
/// ```
/// use xstate_core::builder::Transition;
///
/// let t = Transition::to("review").guard("isTrusted").action("audit").reenter();
/// let targetless = Transition::targetless().action("count");
/// # let _ = (t, targetless);
/// ```
#[derive(Debug, Clone)]
pub struct Transition(pub(super) TransitionConfig);

impl Transition {
    /// A transition to one target (`"b"`, `".child"`, `"#id"`, `"a.b"`).
    #[must_use]
    pub fn to(target: impl Into<String>) -> Self {
        Self::to_all([target.into()])
    }

    /// A transition to several targets (entering parallel regions at once).
    #[must_use]
    pub fn to_all(targets: impl IntoIterator<Item: Into<String>>) -> Self {
        Self(TransitionConfig {
            targets: targets.into_iter().map(Into::into).collect(),
            ..TransitionConfig::default()
        })
    }

    /// A transition without a target: runs its actions and stays in the current state.
    #[must_use]
    pub fn targetless() -> Self {
        Self(TransitionConfig::default())
    }

    /// A forbidden transition: consumes the event and does nothing, hiding ancestors'
    /// handlers for it (`"E": null` in JSON).
    #[must_use]
    pub fn forbidden() -> Self {
        Self(TransitionConfig { forbidden: true, ..TransitionConfig::default() })
    }

    /// Guard the transition (replaces a previous guard).
    #[must_use]
    pub fn guard(mut self, guard: impl Into<Guard>) -> Self {
        self.0.guard = Some(guard.into().0);
        self
    }

    /// Append an action.
    #[must_use]
    pub fn action(mut self, action: impl Into<Action>) -> Self {
        self.0.actions.push(action.into().0);
        self
    }

    /// Append several actions, in order.
    #[must_use]
    pub fn actions(mut self, actions: impl IntoIterator<Item: Into<Action>>) -> Self {
        self.0.actions.extend(actions.into_iter().map(|a| a.into().0));
        self
    }

    /// Exit and re-enter the source state even for a self or descendant transition
    /// (`reenter: true`).
    #[must_use]
    pub fn reenter(mut self) -> Self {
        self.0.reenter = true;
        self
    }

    /// Free-form metadata.
    #[must_use]
    pub fn meta(mut self, meta: impl Into<Value>) -> Self {
        self.0.meta = Some(meta.into());
        self
    }

    /// Human description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.0.description = Some(description.into());
        self
    }
}

impl From<&str> for Transition {
    fn from(target: &str) -> Self {
        Self::to(target)
    }
}

impl From<String> for Transition {
    fn from(target: String) -> Self {
        Self::to(target)
    }
}

/// Anything that can stand for the handler of an event: a bare target, a [`Transition`], or a
/// list of transitions (first match wins).
pub trait IntoTransitions {
    /// The candidate transitions, in priority order.
    fn into_transitions(self) -> Vec<TransitionConfig>;
}

impl IntoTransitions for &str {
    fn into_transitions(self) -> Vec<TransitionConfig> {
        Transition::to(self).into_transitions()
    }
}

impl IntoTransitions for String {
    fn into_transitions(self) -> Vec<TransitionConfig> {
        Transition::to(self).into_transitions()
    }
}

impl IntoTransitions for Transition {
    fn into_transitions(self) -> Vec<TransitionConfig> {
        vec![self.0]
    }
}

impl IntoTransitions for Vec<Transition> {
    fn into_transitions(self) -> Vec<TransitionConfig> {
        self.into_iter().map(|t| t.0).collect()
    }
}

impl<const N: usize> IntoTransitions for [Transition; N] {
    fn into_transitions(self) -> Vec<TransitionConfig> {
        self.into_iter().map(|t| t.0).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Invoke
// ---------------------------------------------------------------------------------------------

/// An `invoke` entry: a service started on entry to a state.
///
/// ```
/// use xstate_core::builder::{Invoke, Transition};
///
/// let invoke = Invoke::new("fetchUser")
///     .id("fetchUser")
///     .input(serde_json::json!({ "userId": 7 }))
///     .on_done(Transition::to("ready").action("saveUser"))
///     .on_error("failed");
/// # let _ = invoke;
/// ```
#[derive(Debug, Clone)]
pub struct Invoke(pub(super) InvokeConfig);

impl Invoke {
    /// Invoke the service registered under `src`.
    #[must_use]
    pub fn new(src: impl Into<String>) -> Self {
        Self(InvokeConfig {
            id: None,
            src: src.into(),
            input: Value::Null,
            system_id: None,
            on_done: Vec::new(),
            on_error: Vec::new(),
        })
    }

    /// Explicit invoke id (defaults to the owning state's id).
    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.0.id = Some(id.into());
        self
    }

    /// Static input passed to the service.
    #[must_use]
    pub fn input(mut self, input: impl Into<Value>) -> Self {
        self.0.input = input.into();
        self
    }

    /// Register the actor under a `systemId`.
    #[must_use]
    pub fn system_id(mut self, system_id: impl Into<String>) -> Self {
        self.0.system_id = Some(system_id.into());
        self
    }

    /// Transitions taken when the service completes.
    #[must_use]
    pub fn on_done(mut self, transitions: impl IntoTransitions) -> Self {
        self.0.on_done = transitions.into_transitions();
        self
    }

    /// Transitions taken when the service fails.
    #[must_use]
    pub fn on_error(mut self, transitions: impl IntoTransitions) -> Self {
        self.0.on_error = transitions.into_transitions();
        self
    }
}

// ---------------------------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------------------------

/// A history pseudo-state: shallow or deep, with an optional default target.
///
/// ```
/// use xstate_core::builder::History;
///
/// let shallow = History::shallow();
/// let deep_with_default = History::deep().target("address");
/// # let _ = (shallow, deep_with_default);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub(super) deep: bool,
    pub(super) target: Option<String>,
}

impl History {
    /// Restores the parent's direct child that was last active.
    #[must_use]
    pub fn shallow() -> Self {
        Self { deep: false, target: None }
    }

    /// Restores the full leaf configuration that was last active.
    #[must_use]
    pub fn deep() -> Self {
        Self { deep: true, target: None }
    }

    /// Default target when no history has been recorded yet.
    #[must_use]
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Delays
// ---------------------------------------------------------------------------------------------

/// The key of an `after` entry or the `delay` of a `raise`: fixed milliseconds or a delay
/// name resolved through the logic registry.
///
/// Integers (`u64`), [`Duration`]s and strings convert into it:
///
/// ```
/// use std::time::Duration;
/// use xstate_core::builder::DelayKey;
///
/// assert_eq!(DelayKey::from(500_u64), DelayKey::Fixed(500));
/// assert_eq!(DelayKey::from(Duration::from_secs(2)), DelayKey::Fixed(2000));
/// assert_eq!(DelayKey::from("BACKOFF"), DelayKey::Named("BACKOFF".into()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelayKey {
    /// A fixed delay in milliseconds.
    Fixed(u64),
    /// A named delay (`MachineLogic::delay` / `delay_fn`).
    Named(String),
}

impl DelayKey {
    /// The raw `after` key as the JSON form spells it (`"500"` or the delay name).
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Fixed(ms) => ms.to_string(),
            Self::Named(name) => name.clone(),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Fixed(ms) => Value::from(*ms),
            Self::Named(name) => Value::String(name.clone()),
        }
    }
}

impl From<u64> for DelayKey {
    fn from(ms: u64) -> Self {
        Self::Fixed(ms)
    }
}

impl From<Duration> for DelayKey {
    fn from(duration: Duration) -> Self {
        Self::Fixed(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    }
}

impl From<&str> for DelayKey {
    fn from(name: &str) -> Self {
        Self::Named(name.to_owned())
    }
}

impl From<String> for DelayKey {
    fn from(name: String) -> Self {
        Self::Named(name)
    }
}
