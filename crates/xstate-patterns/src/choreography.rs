//! [`ChoreographyRouter`]: an [`InboundDispatcher`] over a `type → machine` mapping, the
//! counterpart of the Python `patterns.choreography`.
//!
//! Each participating machine owns its own instances under `<machine id>:<subject>`, so two
//! machines can react to the same business key. Envelope types nobody routes are acked
//! ([`OnUnknown::Ignore`]): a choreography bus carries other services' events, which are not
//! poison. [`PersistedHandler`] is the [`EnvelopeHandler`] Python builds implicitly: load the
//! instance with `persisted`, send the event, save.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use xstate_core::{Context, Machine, MachineLogic};
use xstate_eda::dispatcher::{Dispatch, EnvelopeHandler, HandlerError, OnUnknown};
use xstate_eda::{
    BrokerError, DispatchResult, Envelope, InboundDispatcher, SyncBrokerAdapter, default_event_name,
};
use xstate_persistence::{PersistedOptions, ReceiptFlags, ReceiptJson, StateStore, persisted};

/// Default loop guard of [`ChoreographyRouter::run_until_quiet`].
pub const MAX_ROUNDS: usize = 1000;

/// Deliver envelopes of one type to `machine` as event `event` (default:
/// [`default_event_name`] of the envelope type).
#[derive(Debug, Clone)]
pub struct Route {
    /// The machine that handles the type.
    pub machine: Machine,
    /// Explicit machine event; `None` applies the default naming.
    pub event: Option<String>,
}

impl Route {
    /// Route to `machine` with the default event name.
    #[must_use]
    pub fn new(machine: Machine) -> Self {
        Self { machine, event: None }
    }

    /// Route to `machine` as `event`.
    #[must_use]
    pub fn as_event(machine: Machine, event: impl Into<String>) -> Self {
        Self { machine, event: Some(event.into()) }
    }
}

impl From<Machine> for Route {
    fn from(machine: Machine) -> Self {
        Self::new(machine)
    }
}

impl<S: Into<String>> From<(Machine, S)> for Route {
    fn from((machine, event): (Machine, S)) -> Self {
        Self::as_event(machine, event)
    }
}

/// Errors of [`ChoreographyRouter::run_until_quiet`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChoreographyError {
    /// `max_rounds` was 0.
    #[error("max_rounds must be >= 1, got {0}")]
    InvalidRounds(usize),
    /// Traffic was still flowing after `max_rounds` — usually two machines publishing at each
    /// other forever.
    #[error("choreography did not settle in {0} rounds (an event loop between machines?)")]
    Unsettled(usize),
    /// The broker failed while polling or settling.
    #[error(transparent)]
    Broker(#[from] BrokerError),
}

/// Routes keyed by envelope type.
pub type Routes = HashMap<String, Route>;

/// An [`InboundDispatcher`] over `{envelope type: Route}`.
#[derive(Debug)]
pub struct ChoreographyRouter<H> {
    routes: Arc<Routes>,
    topics: Vec<String>,
    dispatcher: InboundDispatcher<H>,
}

impl<H: EnvelopeHandler> ChoreographyRouter<H> {
    /// A router consuming topic `events`, with Python's dispatcher defaults plus
    /// `key_for = instance_key`, `event_type = event_for` and `on_unknown = Ignore`.
    pub fn new(routes: Routes, handler: H) -> Self {
        Self::with_dispatcher(routes, handler, |d| d)
    }

    /// Like [`ChoreographyRouter::new`], letting `configure` set inbox, dead-letter sink,
    /// attempts, batch size or in-flight limit on the dispatcher before it is used. The router's
    /// own `key_for`, `event_type` and `on_unknown` are applied first and may be overridden.
    pub fn with_dispatcher(
        routes: Routes,
        handler: H,
        configure: impl FnOnce(InboundDispatcher<H>) -> InboundDispatcher<H>,
    ) -> Self {
        let routes = Arc::new(routes);
        let lookup = {
            let routes = routes.clone();
            move |ty: &str| routes.get(ty).map(|r| r.machine.clone())
        };
        let event_for = {
            let routes = routes.clone();
            move |env: &Envelope| event_for(&routes, env)
        };
        let dispatcher = InboundDispatcher::new(lookup, handler)
            .key_for(instance_key)
            .event_type(event_for)
            .on_unknown(OnUnknown::Ignore);
        Self { routes, topics: vec!["events".to_owned()], dispatcher: configure(dispatcher) }
    }

    /// Topics the router consumes (default `["events"]`).
    #[must_use]
    pub fn topics<I, S>(mut self, topics: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.topics = topics.into_iter().map(Into::into).collect();
        self
    }

    /// The routes.
    #[must_use]
    pub fn routes(&self) -> &Routes {
        &self.routes
    }

    /// The underlying dispatcher.
    #[must_use]
    pub fn dispatcher(&self) -> &InboundDispatcher<H> {
        &self.dispatcher
    }

    /// The machine routed for `envelope_type`, or `None` (acked).
    #[must_use]
    pub fn machine_for(&self, envelope_type: &str) -> Option<&Machine> {
        self.routes.get(envelope_type).map(|r| &r.machine)
    }

    /// The machine event an envelope becomes (route event or default).
    #[must_use]
    pub fn event_for(&self, envelope: &Envelope) -> String {
        event_for(&self.routes, envelope)
    }

    /// One dispatcher pass over every topic, waiting up to `timeout` of idle time per topic;
    /// the merged result.
    ///
    /// # Errors
    /// Broker failures.
    pub fn run_once<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        timeout: Duration,
    ) -> Result<DispatchResult, BrokerError> {
        let mut total = DispatchResult::default();
        for topic in &self.topics {
            total.merge(self.dispatcher.run_once_sync(broker, topic, timeout)?);
        }
        Ok(total)
    }

    /// Pump every topic until one round handles nothing; every round's outcomes merged.
    ///
    /// # Errors
    /// [`ChoreographyError::InvalidRounds`] for `max_rounds == 0`,
    /// [`ChoreographyError::Unsettled`] when traffic still flows after `max_rounds`, broker
    /// failures.
    pub fn run_until_quiet<B: SyncBrokerAdapter>(
        &self,
        broker: &B,
        timeout: Duration,
        max_rounds: usize,
    ) -> Result<DispatchResult, ChoreographyError> {
        if max_rounds == 0 {
            return Err(ChoreographyError::InvalidRounds(max_rounds));
        }
        let mut total = DispatchResult::default();
        for _ in 0..max_rounds {
            let round = self.run_once(broker, timeout)?;
            let quiet = round.outcomes.is_empty();
            total.merge(round);
            if quiet {
                return Ok(total);
            }
        }
        Err(ChoreographyError::Unsettled(max_rounds))
    }
}

/// Store key `<machine id>:<subject>` (one business key, many machines). A missing subject
/// renders as `None`, as Python's f-string does.
#[must_use]
pub fn instance_key(envelope: &Envelope, machine: &Machine) -> String {
    format!("{}:{}", machine.id(), envelope.subject.as_deref().unwrap_or("None"))
}

fn event_for(routes: &Routes, envelope: &Envelope) -> String {
    match routes.get(&envelope.ty).and_then(|r| r.event.as_deref()) {
        Some(event) if !event.is_empty() => event.to_owned(),
        _ => default_event_name(&envelope.ty).to_owned(),
    }
}

/// Builds the [`MachineLogic`] for a routed machine (the Python dispatcher passes
/// `machine.logic`; here machines are plain data).
pub type LogicFor<C> = dyn Fn(&Machine) -> MachineLogic<C> + Send + Sync;

/// The handler Python's `InboundDispatcher` has built in: `persisted(store, key, machine)`,
/// `interp.send(event)`, save; the receipt is checked with [`HandlerError::check_receipt`], so
/// a refused event on a finished instance is dead-lettered as `instance_done`.
pub struct PersistedHandler<C: Context> {
    store: Arc<dyn StateStore>,
    logic_for: Box<LogicFor<C>>,
    options: Arc<dyn Fn() -> PersistedOptions<C> + Send + Sync>,
}

impl<C: Context> std::fmt::Debug for PersistedHandler<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistedHandler").finish_non_exhaustive()
    }
}

impl<C: Context> PersistedHandler<C> {
    /// Persist instances in `store`, binding each machine's logic with `logic_for`, with
    /// default [`PersistedOptions`].
    pub fn new(
        store: Arc<dyn StateStore>,
        logic_for: impl Fn(&Machine) -> MachineLogic<C> + Send + Sync + 'static,
    ) -> Self {
        Self { store, logic_for: Box::new(logic_for), options: Arc::new(PersistedOptions::default) }
    }

    /// Build the [`PersistedOptions`] of every cycle with `options` (lock strategy, clock,
    /// observers such as an `OutboxPlugin`).
    #[must_use]
    pub fn options(
        mut self,
        options: impl Fn() -> PersistedOptions<C> + Send + Sync + 'static,
    ) -> Self {
        self.options = Arc::new(options);
        self
    }
}

impl<C: Context> EnvelopeHandler for PersistedHandler<C> {
    fn handle(&self, dispatch: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError> {
        let logic = (self.logic_for)(dispatch.machine);
        let options = (self.options)();
        let event = dispatch.event.clone();
        let receipt = persisted(
            self.store.as_ref(),
            dispatch.key,
            dispatch.machine,
            &logic,
            &options,
            move |interp, _session| {
                let receipt = interp.send(event.clone())?;
                Ok(ReceiptJson::from_receipt(&receipt, ReceiptFlags::default(), None))
            },
        )?;
        HandlerError::check_receipt(receipt)
    }
}
