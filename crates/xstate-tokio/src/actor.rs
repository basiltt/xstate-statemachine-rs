//! The actor task: owns the runtime state, drives the step engine and executes effects.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use xstate_core::config::UnhandledPolicy;
use xstate_core::engine::{self, ActiveState, Effect, StepReport};
use xstate_core::machine::SendTarget;
use xstate_core::{
    Context, Error, Event, EventOrigin, Machine, MachineLogic, Observer, Receipt, Result,
    ServiceImpl, Status,
};

use crate::{Interpreter, Msg, Options, View};

/// Registry of actors addressable by `systemId`, shared by an actor tree.
#[derive(Debug, Clone, Default)]
pub struct System {
    actors: Arc<Mutex<HashMap<String, mpsc::Sender<Msg>>>>,
}

impl System {
    fn register(&self, id: &str, tx: mpsc::Sender<Msg>) -> Result<()> {
        let mut map = self.actors.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.contains_key(id) {
            return Err(Error::ActorSpawning {
                id: id.to_owned(),
                message: "duplicate systemId".into(),
            });
        }
        map.insert(id.to_owned(), tx);
        Ok(())
    }

    fn unregister(&self, id: &str) {
        self.actors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
    }

    fn get(&self, id: &str) -> Option<mpsc::Sender<Msg>> {
        self.actors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).cloned()
    }
}

/// Messages from children and service tasks back to the owning actor.
enum Internal {
    ServiceDone { id: String, result: Result<Value> },
    ChildDone { id: String, output: Value },
    ChildError { id: String, error: String },
    FromChild(Event),
}

struct Timer {
    due: Instant,
    seq: u64,
    event: Event,
    owner: Option<xstate_core::StateId>,
    id: Option<String>,
}

impl PartialEq for Timer {
    fn eq(&self, o: &Self) -> bool {
        self.seq == o.seq
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Timer {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.due.cmp(&self.due).then_with(|| o.seq.cmp(&self.seq))
    }
}

struct Child {
    handle: Interpreter<crate::DynContext>,
    system_id: Option<String>,
}

/// Link to the parent actor, for `sendParent` and completion notifications.
struct ParentLink {
    tx: mpsc::UnboundedSender<Internal>,
    invoke_id: String,
}

struct Core<C: Context> {
    machine: Machine,
    logic: Arc<MachineLogic<C>>,
    state: ActiveState<C>,
    timers: BinaryHeap<Timer>,
    cancelled: HashSet<u64>,
    next_seq: u64,
    services: HashMap<String, JoinHandle<()>>,
    children: HashMap<String, Child>,
    system: System,
    parent: Option<ParentLink>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    observers: Vec<Arc<dyn Observer<C>>>,
    deferred: VecDeque<Event>,
    view_tx: watch::Sender<View>,
    effects: Vec<Effect>,
    stopping: bool,
    finished_notified: bool,
}

pub(crate) fn spawn_root<C: Context>(
    machine: Machine,
    logic: Arc<MachineLogic<C>>,
    options: Options,
    observers: Vec<Arc<dyn Observer<C>>>,
) -> Result<Interpreter<C>> {
    spawn_actor(machine, logic, options, observers, System::default(), None)
}

fn spawn_actor<C: Context>(
    machine: Machine,
    logic: Arc<MachineLogic<C>>,
    options: Options,
    observers: Vec<Arc<dyn Observer<C>>>,
    system: System,
    parent: Option<ParentLink>,
) -> Result<Interpreter<C>> {
    logic.validate(&machine)?;
    let Options { mailbox_capacity, overflow, input } = options;
    let (state, effects, report) = engine::start(&machine, &logic, &input)?;
    let (tx, rx) = mpsc::channel::<Msg>(mailbox_capacity.max(1));
    let (internal_tx, internal_rx) = mpsc::unbounded_channel();
    let (view_tx, view_rx) = watch::channel(view_of(&machine, &state));
    let handle = Interpreter::from_parts(machine.clone(), tx, overflow, view_rx);
    let mut core = Core {
        machine,
        logic,
        state,
        timers: BinaryHeap::new(),
        cancelled: HashSet::new(),
        next_seq: 0,
        services: HashMap::new(),
        children: HashMap::new(),
        system,
        parent,
        internal_tx,
        observers,
        deferred: VecDeque::new(),
        view_tx,
        effects: Vec::with_capacity(8),
        stopping: false,
        finished_notified: false,
    };
    for obs in &core.observers {
        obs.on_start(&core.machine, &core.state, &report);
    }
    core.apply_effects(effects);
    tokio::spawn(run(core, rx, internal_rx));
    Ok(handle)
}

fn view_of<C: Context>(machine: &Machine, state: &ActiveState<C>) -> View {
    View {
        status: state.status(),
        state_ids: state.leaf_ids(machine),
        output: state.output().clone(),
        error: state.error().map(str::to_owned),
    }
}

async fn run<C: Context>(
    mut core: Core<C>,
    mut rx: mpsc::Receiver<Msg>,
    mut internal_rx: mpsc::UnboundedReceiver<Internal>,
) {
    core.publish();
    core.check_finished();
    while !core.stopping {
        let next_due = core.next_due();
        tokio::select! {
            biased;
            internal = internal_rx.recv() => {
                if let Some(msg) = internal {
                    core.on_internal(msg);
                }
            }
            msg = rx.recv() => match msg {
                None => core.shutdown(),
                Some(Msg::Event(event, reply)) => {
                    let outcome = core.process(event);
                    if let Some(reply) = reply {
                        let _ = reply.send(outcome);
                    }
                }
                Some(Msg::Snapshot(reply)) => {
                    let _ = reply.send(core.snapshot());
                }
                Some(Msg::Stop(reply)) => {
                    core.shutdown();
                    let _ = reply.send(());
                }
            },
            () = async { tokio::time::sleep_until(next_due.unwrap_or_else(Instant::now)).await }, if next_due.is_some() => {
                core.fire_due_timers();
            }
        }
    }
    core.publish();
}

impl<C: Context> Core<C> {
    fn publish(&self) {
        let view = view_of(&self.machine, &self.state);
        self.view_tx.send_replace(view);
    }

    fn next_due(&self) -> Option<Instant> {
        self.timers.iter().filter(|t| !self.cancelled.contains(&t.seq)).map(|t| t.due).min()
    }

    fn process(&mut self, event: Event) -> Result<Receipt> {
        if self.state.status() != Status::Running {
            let report = StepReport { unhandled: true, ..StepReport::default() };
            for obs in &self.observers {
                obs.on_event_dropped(&self.machine, &event, "not_running");
            }
            return Ok(Receipt::from_parts(
                self.machine.clone(),
                self.state.leaves(&self.machine),
                &report,
                self.state.status(),
            ));
        }
        if self.machine.policies().strict
            && event.origin == EventOrigin::External
            && self
                .machine
                .declared_events()
                .binary_search_by(|d| d.as_str().cmp(&event.ty))
                .is_err()
        {
            return Err(Error::UnknownEvent {
                event: event.ty.to_string(),
                machine: self.machine.id().to_owned(),
            });
        }
        let kept = event.clone();
        let mut effects = std::mem::take(&mut self.effects);
        effects.clear();
        let outcome =
            engine::step(&self.machine, &self.logic, &mut self.state, event, &mut effects);
        let report = match outcome {
            Ok(report) => report,
            Err(err) => {
                self.effects = effects;
                for obs in &self.observers {
                    obs.on_error(&self.machine, &err);
                }
                self.publish();
                self.check_finished();
                return Err(err);
            }
        };
        for obs in &self.observers {
            obs.on_event(&self.machine, &kept, &report, &self.state);
        }
        self.unhandled_policy(&kept, &report);
        self.apply_effects(effects);
        let receipt = Receipt::from_parts(
            self.machine.clone(),
            self.state.leaves(&self.machine),
            &report,
            self.state.status(),
        );
        self.publish();
        self.check_finished();
        Ok(receipt)
    }

    fn unhandled_policy(&mut self, event: &Event, report: &StepReport) {
        if report.unhandled && event.origin == EventOrigin::External {
            match self.machine.policies().on_unhandled {
                UnhandledPolicy::Ignore => {}
                UnhandledPolicy::Defer => {
                    if self.deferred.len() >= xstate_core::MAX_DEFERRED {
                        self.deferred.pop_front();
                    }
                    for obs in &self.observers {
                        obs.on_event_dropped(&self.machine, event, "deferred");
                    }
                    self.deferred.push_back(event.clone());
                }
                UnhandledPolicy::Error => {
                    self.state.context_mut(); // keep borrowck simple
                    self.fail(format!("unhandled event '{}'", event.ty));
                }
            }
        } else if !report.transitions.is_empty() && !self.deferred.is_empty() {
            let replay: Vec<Event> = self.deferred.drain(..).collect();
            for ev in replay {
                // Re-processing synchronously keeps ordering before newer mailbox events.
                let _ = self.process(ev);
            }
        }
    }

    fn fail(&mut self, message: String) {
        if self.state.status() == Status::Running {
            // Route through the engine's error path so status/error are set consistently.
            let event = Event::error_actor("__unhandled__", Value::String(message));
            let mut effects = Vec::new();
            let _ = engine::step(&self.machine, &self.logic, &mut self.state, event, &mut effects);
        }
    }

    /// Completion / failure: stop children and services and notify the parent. The task keeps
    /// answering `snapshot`/`stop` requests (a finished actor still has a state) until every
    /// handle is dropped or `stop` is called.
    fn check_finished(&mut self) {
        if self.finished_notified {
            return;
        }
        match self.state.status() {
            Status::Running => {}
            Status::Done => {
                self.finished_notified = true;
                let output = self.state.output().clone();
                self.stop_all_children();
                if let Some(parent) = &self.parent {
                    let _ = parent
                        .tx
                        .send(Internal::ChildDone { id: parent.invoke_id.clone(), output });
                }
            }
            Status::Error => {
                self.finished_notified = true;
                let error = self.state.error().unwrap_or("error").to_owned();
                self.stop_all_children();
                if let Some(parent) = &self.parent {
                    let _ = parent
                        .tx
                        .send(Internal::ChildError { id: parent.invoke_id.clone(), error });
                }
            }
            Status::Stopped => {
                self.finished_notified = true;
                self.stop_all_children();
            }
        }
    }

    fn shutdown(&mut self) {
        self.stop_all_children();
        self.state.stop();
        for obs in &self.observers {
            obs.on_stop(&self.machine, &self.state);
        }
        self.stopping = true;
    }

    fn stop_all_children(&mut self) {
        for (_, handle) in self.services.drain() {
            handle.abort();
        }
        for (_, child) in self.children.drain() {
            if let Some(system_id) = &child.system_id {
                self.system.unregister(system_id);
            }
            let tx = child.handle.sender();
            tokio::spawn(async move {
                let (reply, _rx) = oneshot::channel();
                let _ = tx.send(Msg::Stop(reply)).await;
            });
        }
        self.timers.clear();
        self.cancelled.clear();
    }

    fn fire_due_timers(&mut self) {
        let now = Instant::now();
        while let Some(top) = self.timers.peek() {
            if top.due > now {
                break;
            }
            let Some(timer) = self.timers.pop() else { break };
            if self.cancelled.remove(&timer.seq) {
                continue;
            }
            let _ = self.process(timer.event);
            if self.stopping {
                break;
            }
        }
    }

    fn arm(
        &mut self,
        delay_ms: f64,
        event: Event,
        owner: Option<xstate_core::StateId>,
        id: Option<String>,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let due = Instant::now() + std::time::Duration::from_secs_f64(delay_ms.max(0.0) / 1000.0);
        self.timers.push(Timer { due, seq, event, owner, id });
    }

    fn cancel_where(&mut self, pred: impl Fn(&Timer) -> bool) {
        for t in &self.timers {
            if pred(t) {
                self.cancelled.insert(t.seq);
            }
        }
    }

    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::ServiceDone { id, result } => {
                self.services.remove(&id);
                let event = match result {
                    Ok(output) => Event::done_actor(&id, output),
                    Err(err) => Event::error_actor(&id, Value::String(err.to_string())),
                };
                let _ = self.process(event);
            }
            Internal::ChildDone { id, output } => {
                self.remove_child(&id);
                let _ = self.process(Event::done_actor(&id, output));
            }
            Internal::ChildError { id, error } => {
                self.remove_child(&id);
                let _ = self.process(Event::error_actor(&id, Value::String(error)));
            }
            Internal::FromChild(event) => {
                let _ = self.process(event);
            }
        }
    }

    fn remove_child(&mut self, id: &str) {
        if let Some(child) = self.children.remove(id)
            && let Some(system_id) = child.system_id
        {
            self.system.unregister(&system_id);
        }
    }

    fn apply_effects(&mut self, mut effects: Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::ArmTimer { state, delay_ms, event, .. } => {
                    self.arm(delay_ms, event, Some(state), None);
                }
                Effect::CancelTimers(state) => self.cancel_where(|t| t.owner == Some(state)),
                Effect::DelayedRaise { event, delay_ms, id } => self.arm(delay_ms, event, None, id),
                Effect::CancelSend(id) => {
                    self.cancel_where(|t| t.id.as_deref() == Some(id.as_str()));
                }
                Effect::StartInvoke { invoke, input, .. } => {
                    self.start_invoke(&invoke.id, &invoke.src, input, invoke.system_id.as_deref());
                }
                Effect::Spawn { src, id, system_id, input } => {
                    self.start_invoke(&id, &src, input, system_id.as_deref());
                }
                Effect::StopActor(id) => self.stop_actor(&id),
                Effect::SendTo { to, event, delay_ms, id } => {
                    self.send_to(to, event, delay_ms, id.as_deref());
                }
                Effect::Escalate(error) => {
                    if let Some(parent) = &self.parent {
                        let _ = parent.tx.send(Internal::ChildError {
                            id: parent.invoke_id.clone(),
                            error: error.to_string(),
                        });
                    }
                }
                Effect::Emit(value) => {
                    tracing::debug!(target: "xstate", machine = self.machine.id(), event = %value, "emit");
                }
                Effect::Log { expr, label } => {
                    tracing::info!(target: "xstate", machine = self.machine.id(), label = label.as_deref().unwrap_or(""), value = %expr, "log");
                }
                Effect::Raise(_) => {}
                _ => tracing::debug!(target: "xstate", "unsupported effect ignored"),
            }
        }
        self.effects = effects;
    }

    fn start_invoke(&mut self, id: &str, src: &str, input: Value, system_id: Option<&str>) {
        let service = match self.logic.service_impl(src) {
            Ok(s) => s.clone(),
            Err(err) => {
                let _ = self
                    .internal_tx
                    .send(Internal::ServiceDone { id: id.to_owned(), result: Err(err) });
                return;
            }
        };
        match service {
            ServiceImpl::Sync(f) => {
                let result = f(self.state.context(), &input);
                let _ = self.internal_tx.send(Internal::ServiceDone { id: id.to_owned(), result });
            }
            ServiceImpl::Async(f) => {
                let fut = f(self.state.context().clone(), input);
                let tx = self.internal_tx.clone();
                let id = id.to_owned();
                let task_id = id.clone();
                let handle = tokio::spawn(async move {
                    let result = fut.await;
                    let _ = tx.send(Internal::ServiceDone { id: task_id, result });
                });
                self.services.insert(id, handle);
            }
            ServiceImpl::Machine(child_machine) => {
                let parent = ParentLink { tx: self.internal_tx.clone(), invoke_id: id.to_owned() };
                let options = Options { input, ..Options::default() };
                // Children share the logic registry and the context type of their parent.
                match spawn_actor(
                    child_machine,
                    Arc::clone(&self.logic),
                    options,
                    Vec::new(),
                    self.system.clone(),
                    Some(parent),
                ) {
                    Ok(handle) => {
                        if let Some(sys) = system_id
                            && let Err(err) = self.system.register(sys, handle.sender())
                        {
                            let _ = self.internal_tx.send(Internal::ServiceDone {
                                id: id.to_owned(),
                                result: Err(err),
                            });
                            return;
                        }
                        self.children.insert(
                            id.to_owned(),
                            Child {
                                handle: erase(&handle),
                                system_id: system_id.map(str::to_owned),
                            },
                        );
                    }
                    Err(err) => {
                        let _ = self
                            .internal_tx
                            .send(Internal::ServiceDone { id: id.to_owned(), result: Err(err) });
                    }
                }
            }
        }
    }

    fn stop_actor(&mut self, id: &str) {
        if let Some(handle) = self.services.remove(id) {
            handle.abort();
        }
        if let Some(child) = self.children.remove(id) {
            if let Some(system_id) = &child.system_id {
                self.system.unregister(system_id);
            }
            let tx = child.handle.sender();
            tokio::spawn(async move {
                let (reply, _rx) = oneshot::channel();
                let _ = tx.send(Msg::Stop(reply)).await;
            });
        }
    }

    fn send_to(&mut self, to: SendTarget, event: Event, delay_ms: Option<f64>, id: Option<&str>) {
        if let Some(ms) = delay_ms {
            // Delayed sends to other actors are scheduled locally, then routed on expiry; `id`
            // based cancellation of cross-actor sends is not supported yet.
            let _ = id;
            let tx = self.route(&to);
            let delay = std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(tx) = tx {
                    let _ = tx.send(Msg::Event(event, None)).await;
                }
            });
            return;
        }
        match to {
            SendTarget::Parent => {
                if let Some(parent) = &self.parent {
                    let _ = parent.tx.send(Internal::FromChild(event));
                } else {
                    tracing::warn!(target: "xstate", machine = self.machine.id(), "sendParent on a root actor; dropped");
                }
            }
            SendTarget::SelfActor => {
                let _ = self.process(event);
            }
            SendTarget::Named(name) => {
                if let Some(tx) = self.route(&SendTarget::Named(name.clone())) {
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::Event(event, None)).await;
                    });
                } else {
                    tracing::warn!(target: "xstate", machine = self.machine.id(), actor = %name, "sendTo: unknown actor; dropped");
                }
            }
        }
    }

    fn route(&self, to: &SendTarget) -> Option<mpsc::Sender<Msg>> {
        match to {
            SendTarget::Named(name) => {
                self.children.get(name).map(|c| c.handle.sender()).or_else(|| self.system.get(name))
            }
            SendTarget::Parent | SendTarget::SelfActor => None,
        }
    }

    fn snapshot(&self) -> Result<xstate_core::Snapshot> {
        // The async actor shares the snapshot layout through the core helper by rebuilding a
        // synchronous view of its timers; this keeps one implementation of the format.
        let mut snap = xstate_core::snapshot::Snapshot::from_state(&self.machine, &self.state)?;
        let now = Instant::now();
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let mut live: Vec<&Timer> =
            self.timers.iter().filter(|t| !self.cancelled.contains(&t.seq)).collect();
        live.sort_by(|a, b| a.due.cmp(&b.due).then(a.seq.cmp(&b.seq)));
        for t in live {
            let remaining = t.due.saturating_duration_since(now).as_secs_f64() * 1000.0;
            match t.owner {
                Some(owner) => snap.deadlines.push(xstate_core::snapshot::Deadline {
                    state_id: self.machine.node(owner).id.to_string(),
                    entry_seq: 0,
                    due_at_wall: wall + remaining / 1000.0,
                    delay_ms: Value::from(remaining),
                    event_type: xstate_core::snapshot::python_event_name(&t.event.ty),
                }),
                None => snap.scheduled_sends.push(xstate_core::snapshot::ScheduledSend {
                    kind: "event".into(),
                    ty: t.event.ty.to_string(),
                    payload: if t.event.payload.is_null() {
                        Value::Object(serde_json::Map::new())
                    } else {
                        t.event.payload.clone()
                    },
                    remaining_ms: remaining,
                    send_id: t.id.clone(),
                }),
            }
        }
        snap.deferred = self
            .deferred
            .iter()
            .map(|e| serde_json::json!({"kind": "event", "type": e.ty, "payload": e.payload}))
            .collect();
        Ok(snap)
    }
}

/// Children are typed with the parent's context type; the handle stored in `Child` only needs
/// the mailbox, so the type parameter is erased to the dynamic context.
fn erase<C: Context>(handle: &Interpreter<C>) -> Interpreter<crate::DynContext> {
    Interpreter::from_parts(
        handle.machine().clone(),
        handle.sender(),
        crate::Overflow::Block,
        handle.subscribe(),
    )
}
