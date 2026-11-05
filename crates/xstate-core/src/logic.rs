//! The logic registry: the named guards, actions, services and delays a machine refers to.
//!
//! JSON machines name their implementations (`"guard": "isUnlocked"`); Rust supplies them
//! here. Every name is checked when the registry is validated against a
//! [`Machine`], so a typo surfaces at build time rather than when
//! the transition is first taken.

use foldhash::HashMap;
use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::context::Context;
use crate::engine::Effect;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::machine::{ActionDef, Delay, Guard, Machine};

/// What an action implementation can see and do.
pub struct ActionScope<'a, C> {
    /// Mutable context.
    pub context: &'a mut C,
    /// The event being processed.
    pub event: &'a Event,
    /// Static `params` from the configuration (`null` when absent).
    pub params: &'a Value,
    pub(crate) effects: &'a mut Vec<Effect>,
}

impl<C> ActionScope<'_, C> {
    /// Queue an event to this machine; it is processed before the current macrostep ends.
    pub fn raise(&mut self, event: Event) {
        self.effects
            .push(Effect::Raise(Event { origin: crate::event::EventOrigin::Internal, ..event }));
    }

    /// Emit an event to external listeners (`actor.on(type, ...)` in XState).
    pub fn emit(&mut self, event: Value) {
        self.effects.push(Effect::Emit(event));
    }
}

impl<C> fmt::Debug for ActionScope<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionScope").field("event", &self.event.ty).finish_non_exhaustive()
    }
}

/// Boxed action implementation.
pub type ActionFn<C> = Arc<dyn Fn(&mut ActionScope<'_, C>) -> Result<()> + Send + Sync>;
/// Boxed guard implementation: `(context, event, params) -> bool`.
pub type GuardFn<C> = Arc<dyn Fn(&C, &Event, &Value) -> Result<bool> + Send + Sync>;
/// Boxed dynamic delay: `(context, event) -> milliseconds`.
pub type DelayFn<C> = Arc<dyn Fn(&C, &Event) -> Result<f64> + Send + Sync>;
/// Boxed synchronous service: `(context, input) -> output`.
pub type SyncServiceFn<C> = Arc<dyn Fn(&C, &Value) -> Result<Value> + Send + Sync>;

/// A delay implementation.
pub enum DelayImpl<C> {
    /// Constant milliseconds.
    Fixed(f64),
    /// Computed from context and event.
    Dynamic(DelayFn<C>),
}

/// A service implementation.
pub enum ServiceImpl<C> {
    /// Runs to completion inline when the invoking state is entered; its result becomes the
    /// `xstate.done.actor.<id>` output, an error becomes `xstate.error.actor.<id>`.
    Sync(SyncServiceFn<C>),
    /// Runs as a future (async interpreters only). The future receives a clone of the context
    /// and the resolved input; its output / error complete the invocation like `Sync`.
    Async(AsyncServiceFn<C>),
    /// Runs a child machine as an actor (async interpreters only). Its output on completion
    /// becomes the `xstate.done.actor.<id>` output; `sendTo`/`sendParent` address it by id.
    Machine(Machine),
}

/// A boxed future returned by an async service.
pub type ServiceFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>>;
/// Boxed asynchronous service factory: `(context, input) -> future`.
pub type AsyncServiceFn<C> = Arc<dyn Fn(C, Value) -> ServiceFuture + Send + Sync>;

impl<C> Clone for DelayImpl<C> {
    fn clone(&self) -> Self {
        match self {
            Self::Fixed(ms) => Self::Fixed(*ms),
            Self::Dynamic(f) => Self::Dynamic(Arc::clone(f)),
        }
    }
}

impl<C> Clone for ServiceImpl<C> {
    fn clone(&self) -> Self {
        match self {
            Self::Sync(f) => Self::Sync(Arc::clone(f)),
            Self::Async(f) => Self::Async(Arc::clone(f)),
            Self::Machine(m) => Self::Machine(m.clone()),
        }
    }
}

impl<C> fmt::Debug for DelayImpl<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(ms) => write!(f, "Fixed({ms})"),
            Self::Dynamic(_) => f.write_str("Dynamic(..)"),
        }
    }
}

impl<C> fmt::Debug for ServiceImpl<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(_) => f.write_str("Sync(..)"),
            Self::Async(_) => f.write_str("Async(..)"),
            Self::Machine(m) => write!(f, "Machine({})", m.id()),
        }
    }
}

fn sorted_names<V>(map: &HashMap<String, V>) -> Vec<&str> {
    let mut v: Vec<&str> = map.keys().map(String::as_str).collect();
    v.sort_unstable();
    v
}

/// Registry of implementations for one machine.
pub struct MachineLogic<C> {
    actions: HashMap<String, ActionFn<C>>,
    /// Some registered action is spelled like a built-in (`log`, `cancel`, ...): the engine
    /// then looks the built-in's name up here first (user logic wins over built-ins).
    shadows_builtin: bool,
    guards: HashMap<String, GuardFn<C>>,
    services: HashMap<String, ServiceImpl<C>>,
    delays: HashMap<String, DelayImpl<C>>,
}

impl<C> Default for MachineLogic<C> {
    fn default() -> Self {
        Self {
            actions: HashMap::default(),
            shadows_builtin: false,
            guards: HashMap::default(),
            services: HashMap::default(),
            delays: HashMap::default(),
        }
    }
}

impl<C> Clone for MachineLogic<C> {
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            shadows_builtin: self.shadows_builtin,
            guards: self.guards.clone(),
            services: self.services.clone(),
            delays: self.delays.clone(),
        }
    }
}

impl<C> fmt::Debug for MachineLogic<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachineLogic")
            .field("actions", &sorted_names(&self.actions))
            .field("shadows_builtin", &self.shadows_builtin)
            .field("guards", &sorted_names(&self.guards))
            .field("services", &sorted_names(&self.services))
            .field("delays", &sorted_names(&self.delays))
            .finish()
    }
}

impl MachineLogic<crate::context::DynContext> {
    /// Empty registry for machines with a JSON (`serde_json::Value`) context — the common case
    /// for machines loaded from the Stately editor.
    #[must_use]
    pub fn dynamic() -> Self {
        Self::default()
    }
}

impl<C: Context> MachineLogic<C> {
    /// Empty registry. Use [`MachineLogic::dynamic`] when the context type cannot be inferred.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an action.
    #[must_use]
    pub fn action(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&mut ActionScope<'_, C>) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        self.shadows_builtin |= crate::machine::builtin::canonical(&name).is_some();
        self.actions.insert(name, Arc::new(f));
        self
    }

    /// Names of the registered actions, in no particular order.
    pub fn action_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.actions.keys().map(String::as_str)
    }

    /// Register a guard.
    #[must_use]
    pub fn guard(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&C, &Event, &Value) -> Result<bool> + Send + Sync + 'static,
    ) -> Self {
        self.guards.insert(name.into(), Arc::new(f));
        self
    }

    /// Register an asynchronous service (usable with the tokio interpreter). The closure
    /// receives a clone of the context and the invoke `input` and returns a future.
    ///
    /// ```ignore
    /// logic.async_service("fetchUser", |_ctx, input| Box::pin(async move { Ok(input) }))
    /// ```
    #[must_use]
    pub fn async_service(
        mut self,
        name: impl Into<String>,
        f: impl Fn(C, Value) -> ServiceFuture + Send + Sync + 'static,
    ) -> Self {
        self.services.insert(name.into(), ServiceImpl::Async(Arc::new(f)));
        self
    }

    /// Register a child machine as an invokable actor (usable with the tokio interpreter).
    #[must_use]
    pub fn machine_service(mut self, name: impl Into<String>, machine: Machine) -> Self {
        self.services.insert(name.into(), ServiceImpl::Machine(machine));
        self
    }

    /// Look up a service implementation by name.
    ///
    /// # Errors
    /// [`Error::ImplementationMissing`] when the name is unknown.
    pub fn service_impl(&self, name: &str) -> Result<&ServiceImpl<C>> {
        self.get_service(name)
    }

    /// Register a synchronous service.
    #[must_use]
    pub fn service(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&C, &Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        self.services.insert(name.into(), ServiceImpl::Sync(Arc::new(f)));
        self
    }

    /// Register a fixed delay in milliseconds.
    #[must_use]
    pub fn delay(mut self, name: impl Into<String>, ms: f64) -> Self {
        self.delays.insert(name.into(), DelayImpl::Fixed(ms));
        self
    }

    /// Register a computed delay.
    #[must_use]
    pub fn delay_fn(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&C, &Event) -> Result<f64> + Send + Sync + 'static,
    ) -> Self {
        self.delays.insert(name.into(), DelayImpl::Dynamic(Arc::new(f)));
        self
    }

    pub(crate) fn get_action(&self, name: &str) -> Result<&ActionFn<C>> {
        self.actions
            .get(name)
            .ok_or_else(|| Error::ImplementationMissing { kind: "action", name: name.to_owned() })
    }

    /// The user action overriding the built-in spelled `name`, if one is registered. Cheap
    /// when no registered action is spelled like a built-in.
    #[inline]
    pub(crate) fn builtin_override(&self, name: &str) -> Option<&ActionFn<C>> {
        if self.shadows_builtin { self.actions.get(name) } else { None }
    }

    pub(crate) fn get_guard(&self, name: &str) -> Result<&GuardFn<C>> {
        self.guards
            .get(name)
            .ok_or_else(|| Error::ImplementationMissing { kind: "guard", name: name.to_owned() })
    }

    pub(crate) fn get_service(&self, name: &str) -> Result<&ServiceImpl<C>> {
        self.services
            .get(name)
            .ok_or_else(|| Error::ImplementationMissing { kind: "service", name: name.to_owned() })
    }

    pub(crate) fn get_delay(&self, name: &str) -> Result<&DelayImpl<C>> {
        self.delays
            .get(name)
            .ok_or_else(|| Error::ImplementationMissing { kind: "delay", name: name.to_owned() })
    }

    /// Check that every name the machine references has an implementation.
    ///
    /// # Errors
    /// The first [`Error::ImplementationMissing`] found, in document order.
    pub fn validate(&self, machine: &Machine) -> Result<()> {
        fn walk_actions<C: Context>(logic: &MachineLogic<C>, actions: &[ActionDef]) -> Result<()> {
            for a in actions {
                match a {
                    ActionDef::User { name, .. } => {
                        logic.get_action(name)?;
                    }
                    ActionDef::Builtin {
                        builtin: crate::machine::Builtin::Choose(branches),
                        ..
                    } => {
                        for b in branches {
                            if let Some(g) = &b.guard {
                                walk_guard(logic, g)?;
                            }
                            walk_actions(logic, &b.actions)?;
                        }
                    }
                    ActionDef::Builtin { name, .. } if logic.builtin_override(name).is_some() => {}
                    ActionDef::Builtin {
                        builtin:
                            crate::machine::Builtin::Raise { delay: Some(Delay::Named(d)), .. }
                            | crate::machine::Builtin::SendTo { delay: Some(Delay::Named(d)), .. },
                        ..
                    } => {
                        logic.get_delay(d)?;
                    }
                    ActionDef::Builtin { .. } => {}
                }
            }
            Ok(())
        }
        fn walk_guard<C: Context>(logic: &MachineLogic<C>, guard: &Guard) -> Result<()> {
            match guard {
                Guard::Named { name, .. } => logic.get_guard(name).map(|_| ()),
                Guard::StateIn { .. } => Ok(()),
                Guard::Not(inner) => walk_guard(logic, inner),
                Guard::And(list) | Guard::Or(list) => {
                    list.iter().try_for_each(|g| walk_guard(logic, g))
                }
            }
        }
        if !machine.references_logic() {
            // No action, guard, invoke or delay to look up.
            return Ok(());
        }
        for node in machine.states() {
            walk_actions(self, &node.entry)?;
            walk_actions(self, &node.exit)?;
            for inv in &node.invokes {
                self.get_service(&inv.src)?;
            }
            for after in &node.after {
                if let Delay::Named(name) = &after.delay {
                    self.get_delay(name)?;
                }
            }
        }
        for t in machine.transitions() {
            walk_actions(self, &t.actions)?;
            if let Some(g) = &t.guard {
                walk_guard(self, g)?;
            }
        }
        Ok(())
    }
}
