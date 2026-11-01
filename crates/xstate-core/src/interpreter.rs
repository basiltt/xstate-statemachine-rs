//! The synchronous interpreter: drives the step engine inline, owns the timer heap and runs
//! synchronous services. Single-owner by design; events are processed to completion inside
//! `send`, and timers fire from `tick` / `advance`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::clock::{Clock, RealClock, SimulatedClock};
use crate::context::{Context, DynContext};
use crate::engine::{self, ActiveState, Effect, Status, StepReport};
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::ids::StateId;
use crate::logic::{MachineLogic, ServiceImpl};
use crate::machine::Machine;
use crate::snapshot::{self, Deadline, ScheduledSend, Snapshot};

/// Outcome of one `send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Active leaf state ids after the event (and everything it triggered) was processed.
    pub state_ids: Vec<String>,
    /// Whether the delivered event changed configuration or context.
    pub changed: bool,
    /// Whether no transition matched the delivered event.
    pub unhandled: bool,
    /// User actions executed while processing the event and its consequences, in order.
    pub actions: Vec<String>,
    /// Status after processing.
    pub status: Status,
}

#[derive(Debug)]
struct Timer {
    due_ms: f64,
    seq: u64,
    event: Event,
    owner: Option<StateId>,
    id: Option<String>,
    delay_ms: f64,
    entry_seq: Option<u64>,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    // Reversed so the BinaryHeap pops the earliest due timer, FIFO within the same instant.
    fn cmp(&self, other: &Self) -> Ordering {
        other.due_ms.total_cmp(&self.due_ms).then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Synchronous interpreter over a compiled [`Machine`].
pub struct SyncInterpreter<C: Context = DynContext> {
    machine: Machine,
    logic: Arc<MachineLogic<C>>,
    clock: Arc<dyn Clock>,
    input: Value,
    state: Option<ActiveState<C>>,
    queue: VecDeque<Event>,
    timers: BinaryHeap<Timer>,
    cancelled: HashSet<u64>,
    next_seq: u64,
    processing: bool,
    emitted: Vec<Value>,
    logs: Vec<(Option<String>, Value)>,
}

impl<C: Context> std::fmt::Debug for SyncInterpreter<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncInterpreter")
            .field("machine", &self.machine.id())
            .field("status", &self.status())
            .field("queued", &self.queue.len())
            .field("timers", &self.timers.len())
            .finish_non_exhaustive()
    }
}

impl<C: Context> SyncInterpreter<C> {
    /// Create an interpreter with a real clock. Call [`start`](Self::start) before sending.
    #[must_use]
    pub fn new(machine: Machine, logic: MachineLogic<C>) -> Self {
        Self::with_clock(machine, logic, Arc::new(RealClock::new()))
    }

    /// Create an interpreter driven by the given clock.
    #[must_use]
    pub fn with_clock(machine: Machine, logic: MachineLogic<C>, clock: Arc<dyn Clock>) -> Self {
        Self {
            machine,
            logic: Arc::new(logic),
            clock,
            input: Value::Null,
            state: None,
            queue: VecDeque::new(),
            timers: BinaryHeap::new(),
            cancelled: HashSet::new(),
            next_seq: 0,
            processing: false,
            emitted: Vec::new(),
            logs: Vec::new(),
        }
    }

    /// Set the actor input passed to the context factory.
    #[must_use]
    pub fn with_input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    /// Validate the logic against the machine and enter the initial configuration. The
    /// receipt lists the entry actions that ran.
    ///
    /// # Errors
    /// Missing implementations, failing entry actions, or a second `start`.
    pub fn start(&mut self) -> Result<Receipt> {
        if self.state.is_some() {
            return Err(Error::InvalidLifecycle {
                status: self.status().as_str(),
                message: "already started".into(),
            });
        }
        self.logic.validate(&self.machine)?;
        let (state, effects, mut report) = engine::start(&self.machine, &self.logic, &self.input)?;
        self.state = Some(state);
        self.apply_effects(effects);
        let rest = self.drain()?;
        report.actions.extend(rest.actions);
        report.changed = true;
        Ok(self.receipt(&report))
    }

    /// Deliver an event and process it, plus everything it triggers (timers already due,
    /// synchronous service completions), to completion.
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] before `start`; [`Error::UnknownEvent`] under `strict` for
    /// an undeclared event; logic errors according to the machine's policies.
    pub fn send(&mut self, event: impl Into<Event>) -> Result<Receipt> {
        let event = event.into();
        let state = self.state.as_ref().ok_or_else(|| Error::InvalidLifecycle {
            status: "uninitialized",
            message: "send before start".into(),
        })?;
        if self.machine.policies().strict
            && event.origin == EventOrigin::External
            && self.machine.declared_events().binary_search(&event.ty).is_err()
        {
            return Err(Error::UnknownEvent {
                event: event.ty,
                machine: self.machine.id().to_owned(),
            });
        }
        let _ = state;
        self.queue.push_back(event);
        if self.processing {
            // Re-entrant send from inside an action: queued, processed by the outer loop.
            return Ok(self.receipt(&StepReport::default()));
        }
        let report = self.drain()?;
        Ok(self.receipt(&report))
    }

    /// Fire every timer that is due according to the clock, in due order. The receipt covers
    /// everything processed.
    ///
    /// # Errors
    /// Logic errors raised while processing the timer events.
    pub fn tick(&mut self) -> Result<Receipt> {
        if self.state.is_none() || self.processing {
            return Ok(self.receipt(&StepReport::default()));
        }
        let report = self.drain()?;
        Ok(self.receipt(&report))
    }

    /// Advance a [`SimulatedClock`] and fire what became due. Pass the same clock the
    /// interpreter was constructed with.
    ///
    /// # Errors
    /// Logic errors raised while processing the timer events.
    pub fn advance(&mut self, clock: &SimulatedClock, ms: f64) -> Result<Receipt> {
        clock.increment(ms);
        self.tick()
    }

    /// Stop the machine: pending events and timers are dropped.
    pub fn stop(&mut self) {
        if let Some(state) = self.state.as_mut().filter(|s| s.status == Status::Running) {
            state.status = Status::Stopped;
        }
        self.queue.clear();
        self.timers.clear();
        self.cancelled.clear();
    }

    /// Current status (`Running` before `start` is reported as uninitialised `Stopped`).
    #[must_use]
    pub fn status(&self) -> Status {
        self.state.as_ref().map_or(Status::Stopped, |s| s.status)
    }

    /// The compiled machine.
    #[must_use]
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Runtime state, once started.
    #[must_use]
    pub fn state(&self) -> Option<&ActiveState<C>> {
        self.state.as_ref()
    }

    /// Context, once started.
    #[must_use]
    pub fn context(&self) -> Option<&C> {
        self.state.as_ref().map(ActiveState::context)
    }

    /// Active leaf state ids, sorted.
    #[must_use]
    pub fn state_ids(&self) -> Vec<String> {
        self.state.as_ref().map(|s| s.leaf_ids(&self.machine)).unwrap_or_default()
    }

    /// `true` when the state with this full id (or custom `#id`) is active.
    #[must_use]
    pub fn matches(&self, state_id: &str) -> bool {
        match (self.state.as_ref(), self.machine.state_by_id(state_id)) {
            (Some(s), Some(id)) => s.is_active(id),
            _ => false,
        }
    }

    /// Events emitted through the `emit` built-in since the last call (drains the buffer).
    pub fn take_emitted(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.emitted)
    }

    /// `log` built-in records since the last call (drains the buffer).
    pub fn take_logs(&mut self) -> Vec<(Option<String>, Value)> {
        std::mem::take(&mut self.logs)
    }

    /// Pending timers (for diagnostics).
    #[must_use]
    pub fn pending_timers(&self) -> usize {
        self.timers.iter().filter(|t| !self.cancelled.contains(&t.seq)).count()
    }

    fn receipt(&self, report: &StepReport) -> Receipt {
        Receipt {
            state_ids: self.state_ids(),
            changed: report.changed,
            unhandled: report.unhandled,
            actions: report.actions.clone(),
            status: self.status(),
        }
    }

    /// Process queued events and due timers until nothing is left. Returns the merged report
    /// of everything processed.
    fn drain(&mut self) -> Result<StepReport> {
        self.processing = true;
        let result = self.drain_inner();
        self.processing = false;
        result
    }

    fn drain_inner(&mut self) -> Result<StepReport> {
        let mut merged = StepReport::default();
        let mut first = true;
        loop {
            let event = match self.queue.pop_front() {
                Some(ev) => ev,
                None => match self.pop_due_timer() {
                    Some(ev) => ev,
                    None => break,
                },
            };
            let Some(state) = self.state.as_mut() else { break };
            if state.status != Status::Running {
                continue;
            }
            let mut effects = Vec::new();
            let report =
                engine::step(&self.machine, &self.logic, state, event.clone(), &mut effects)?;
            if first {
                merged.changed = report.changed;
                merged.unhandled = report.unhandled;
                first = false;
            } else {
                merged.changed |= report.changed;
            }
            merged.actions.extend(report.actions);
            merged.transitions.extend(report.transitions);
            merged.microsteps += report.microsteps;
            if report.failed.is_some() {
                merged.failed = report.failed;
            }
            self.apply_effects(effects);
        }
        Ok(merged)
    }

    fn pop_due_timer(&mut self) -> Option<Event> {
        let now = self.clock.now_ms();
        while let Some(top) = self.timers.peek() {
            if top.due_ms > now {
                return None;
            }
            let timer = self.timers.pop()?;
            if self.cancelled.remove(&timer.seq) {
                continue;
            }
            return Some(timer.event);
        }
        None
    }

    fn arm(&mut self, delay_ms: f64, event: Event, owner: Option<StateId>, id: Option<String>) {
        self.arm_with(delay_ms, delay_ms, event, owner, id, None);
    }

    /// Arm a timer whose remaining time differs from its declared delay (snapshot restore).
    fn arm_with(
        &mut self,
        remaining_ms: f64,
        delay_ms: f64,
        event: Event,
        owner: Option<StateId>,
        id: Option<String>,
        entry_seq: Option<u64>,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.timers.push(Timer {
            due_ms: self.clock.now_ms() + remaining_ms.max(0.0),
            seq,
            event,
            owner,
            id,
            delay_ms,
            entry_seq,
        });
    }

    fn cancel_where(&mut self, pred: impl Fn(&Timer) -> bool) {
        for t in &self.timers {
            if pred(t) {
                self.cancelled.insert(t.seq);
            }
        }
    }

    fn apply_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ArmTimer { state, delay_ms, event, entry_seq, .. } => {
                    self.arm_with(delay_ms, delay_ms, event, Some(state), None, Some(entry_seq));
                }
                Effect::CancelTimers(state) => self.cancel_where(|t| t.owner == Some(state)),
                Effect::DelayedRaise { event, delay_ms, id } => self.arm(delay_ms, event, None, id),
                Effect::CancelSend(id) => {
                    self.cancel_where(|t| t.id.as_deref() == Some(id.as_str()));
                }
                Effect::StartInvoke { invoke, input, .. } => {
                    self.run_service(&invoke.id, &invoke.src, &input);
                }
                // Synchronous services have already completed by the time they are stopped.
                Effect::StopActor(_) | Effect::Raise(_) => {}
                Effect::Emit(v) => self.emitted.push(v),
                Effect::Log { expr, label } => {
                    tracing::info!(target: "xstate", label = label.as_deref().unwrap_or(""), value = %expr, "log");
                    self.logs.push((label, expr));
                }
                Effect::SendTo { to, event, .. } => {
                    tracing::warn!(target: "xstate", ?to, event = %event.ty, "sendTo without an actor system; dropped");
                }
                Effect::Spawn { id, .. } => {
                    tracing::warn!(target: "xstate", id = %id, "spawnChild is not supported by SyncInterpreter yet; dropped");
                }
                Effect::Escalate(err) => {
                    tracing::warn!(target: "xstate", error = %err, "escalate without a parent; dropped");
                }
            }
        }
    }

    fn run_service(&mut self, id: &str, src: &str, input: &Value) {
        let Some(state) = self.state.as_ref() else { return };
        let outcome = match self.logic.get_service(src) {
            Ok(ServiceImpl::Sync(f)) => f(&state.context, input),
            Err(err) => Err(err),
        };
        let event = match outcome {
            Ok(output) => Event::done_actor(id, output),
            Err(err) => Event::error_actor(id, Value::String(err.to_string())),
        };
        self.queue.push_back(event);
    }
}

/// How `after` timers found in a snapshot are re-armed on restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartTimers {
    /// Leave them parked (nothing fires).
    Parked,
    /// Re-arm with the declared delay from now.
    Restart,
    /// Re-arm with the remaining time (`due_at_wall - now`), clamped to the declared delay.
    #[default]
    Resume,
}

/// Options for [`SyncInterpreter::from_snapshot`].
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Refuse a snapshot whose `machine_hash` differs from this machine's (default `true`).
    pub verify_hash: Option<bool>,
    /// Timer policy.
    pub restart_timers: RestartTimers,
}

impl<C: Context> SyncInterpreter<C> {
    /// Capture the runtime state in the Python-compatible layout v4.
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] before `start`.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let state = self.state.as_ref().ok_or_else(|| Error::InvalidLifecycle {
            status: "uninitialized",
            message: "snapshot before start".into(),
        })?;
        let machine = &self.machine;
        let now_ms = self.clock.now_ms();
        let wall = self.clock.wall_now();
        let mut deadlines = Vec::new();
        let mut scheduled_sends = Vec::new();
        let mut live: Vec<&Timer> =
            self.timers.iter().filter(|t| !self.cancelled.contains(&t.seq)).collect();
        live.sort_by(|a, b| a.due_ms.total_cmp(&b.due_ms).then(a.seq.cmp(&b.seq)));
        for t in live {
            let remaining = (t.due_ms - now_ms).max(0.0);
            match (t.owner, t.entry_seq) {
                (Some(owner), Some(entry_seq)) => deadlines.push(Deadline {
                    state_id: machine.node(owner).id.clone(),
                    entry_seq,
                    due_at_wall: wall + remaining / 1000.0,
                    delay_ms: json_number(t.delay_ms),
                    event_type: snapshot::python_event_name(&t.event.ty),
                }),
                _ => scheduled_sends.push(ScheduledSend {
                    kind: "event".into(),
                    ty: t.event.ty.clone(),
                    payload: if t.event.payload.is_null() {
                        Value::Object(Map::new())
                    } else {
                        t.event.payload.clone()
                    },
                    remaining_ms: remaining,
                    send_id: t.id.clone(),
                }),
            }
        }
        let active = |s: StateId| state.is_active(s);
        let mut configuration: Vec<String> =
            state.configuration().map(|s| machine.node(s).id.clone()).collect();
        configuration.sort();
        let mut history = indexmap::IndexMap::new();
        let mut recorded: Vec<(&StateId, &Vec<StateId>)> = state.history().iter().collect();
        recorded.sort_by_key(|(k, _)| **k);
        for (parent, states) in recorded {
            let mut ids: Vec<String> = states.iter().map(|s| machine.node(*s).id.clone()).collect();
            ids.sort();
            history.insert(machine.node(*parent).id.clone(), ids);
        }
        let pending_events = self.queue.iter().map(event_record).collect();
        Ok(Snapshot {
            version: crate::SNAPSHOT_VERSION,
            machine_id: machine.id().to_owned(),
            machine_hash: machine.structure_hash().to_owned(),
            machine_version: machine.version().map(str::to_owned),
            taken_at: wall,
            status: state.status(),
            context: serde_json::to_value(state.context())?,
            state_ids: state.leaf_ids(machine),
            value: snapshot::state_value(machine, &active, StateId::ROOT),
            configuration,
            output: state.output().clone(),
            error: state.error().map(str::to_owned),
            chain_trips: 0,
            last_chain_error: None,
            deferred: Vec::new(),
            pending_events,
            scheduled_sends,
            deadlines,
            history,
            actors: Map::new(),
            system: Map::new(),
        })
    }

    /// [`snapshot`](Self::snapshot) serialised to JSON text.
    ///
    /// # Errors
    /// See [`snapshot`](Self::snapshot).
    pub fn snapshot_json(&self) -> Result<String> {
        self.snapshot()?.to_json()
    }

    /// Rebuild an interpreter from a snapshot. Entry actions, invokes and timers do **not**
    /// re-run; timers follow `options.restart_timers`.
    ///
    /// # Errors
    /// [`Error::Snapshot`] when the snapshot belongs to another machine, its hash differs
    /// (unless `verify_hash` is `false`), or its configuration is illegal for this machine.
    pub fn from_snapshot(
        machine: Machine,
        logic: MachineLogic<C>,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock>,
        options: &RestoreOptions,
    ) -> Result<Self> {
        if snapshot.machine_id != machine.id() {
            return Err(Error::Snapshot(format!(
                "snapshot belongs to machine '{}', not '{}'",
                snapshot.machine_id,
                machine.id()
            )));
        }
        if options.verify_hash.unwrap_or(true) && snapshot.machine_hash != machine.structure_hash()
        {
            return Err(Error::Snapshot(format!(
                "machine structure changed since the snapshot was taken (hash {} vs {})",
                snapshot.machine_hash,
                machine.structure_hash()
            )));
        }
        let configuration = snapshot::resolve_configuration(&machine, &snapshot.configuration)?;
        let mut history = HashMap::new();
        for (parent, ids) in &snapshot.history {
            let p = machine.state_by_id(parent).ok_or_else(|| Error::StateNotFound {
                target: parent.clone(),
                from: "snapshot.history".into(),
            })?;
            let mut states = Vec::with_capacity(ids.len());
            for id in ids {
                states.push(machine.state_by_id(id).ok_or_else(|| Error::StateNotFound {
                    target: id.clone(),
                    from: "snapshot.history".into(),
                })?);
            }
            states.sort();
            history.insert(p, states);
        }
        let context: C = serde_json::from_value(snapshot.context.clone())
            .map_err(|e| Error::Snapshot(format!("context does not fit the context type: {e}")))?;
        let state = ActiveState::restore(&machine, &configuration, context, history, snapshot);

        let mut interp = Self::with_clock(machine, logic, clock);
        interp.logic.validate(&interp.machine)?;
        interp.state = Some(state);
        for ev in &snapshot.pending_events {
            if let Some(event) = event_from_record(ev) {
                interp.queue.push_back(event);
            }
        }
        let now_wall = interp.clock.wall_now();
        for d in &snapshot.deadlines {
            let Some(owner) = interp.machine.state_by_id(&d.state_id) else {
                return Err(Error::StateNotFound {
                    target: d.state_id.clone(),
                    from: "snapshot.deadlines".into(),
                });
            };
            let delay_ms = d.delay_ms.as_f64().unwrap_or(0.0);
            let remaining = match options.restart_timers {
                RestartTimers::Parked => continue,
                RestartTimers::Restart => delay_ms,
                RestartTimers::Resume => ((d.due_at_wall - now_wall) * 1000.0).clamp(0.0, delay_ms),
            };
            let event = Event::engine(
                snapshot::engine_event_name(&d.event_type),
                Value::Null,
                EventOrigin::After,
            );
            interp.arm_with(remaining, delay_ms, event, Some(owner), None, Some(d.entry_seq));
        }
        for s in &snapshot.scheduled_sends {
            let payload = if s.payload.as_object().is_some_and(Map::is_empty) {
                Value::Null
            } else {
                s.payload.clone()
            };
            let event = Event::engine(s.ty.clone(), payload, EventOrigin::Internal);
            interp.arm_with(s.remaining_ms, s.remaining_ms, event, None, s.send_id.clone(), None);
        }
        Ok(interp)
    }
}

fn json_number(ms: f64) -> Value {
    if ms.fract() == 0.0 && ms.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        Value::from(ms as i64)
    } else {
        Value::from(ms)
    }
}

fn event_record(event: &Event) -> Value {
    let mut map = Map::new();
    map.insert("kind".into(), Value::String("event".into()));
    map.insert("type".into(), Value::String(event.ty.clone()));
    if !event.payload.is_null() {
        map.insert("payload".into(), event.payload.clone());
    }
    Value::Object(map)
}

fn event_from_record(record: &Value) -> Option<Event> {
    let ty = record.get("type")?.as_str()?;
    let payload = record.get("payload").cloned().unwrap_or(Value::Null);
    Some(Event::engine(snapshot::engine_event_name(ty), payload, EventOrigin::External))
}

impl From<&str> for Event {
    fn from(ty: &str) -> Self {
        Event::new(ty)
    }
}

impl From<String> for Event {
    fn from(ty: String) -> Self {
        Event::new(ty)
    }
}
