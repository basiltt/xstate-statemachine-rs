//! The synchronous interpreter: drives the step engine inline, owns the timer heap and runs
//! synchronous services. Single-owner by design; events are processed to completion inside
//! `send`, and timers fire from `tick` / `advance`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::{Arc, OnceLock};

use foldhash::HashSet;

use serde_json::Value;

use crate::clock::{Clock, RealClock, SimulatedClock};
use crate::config::UnhandledPolicy;
use crate::context::{Context, DynContext};
use crate::engine::{self, ActiveState, Effect, Status, StepReport};
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::ids::StateId;
use crate::logic::{MachineLogic, ServiceImpl};
use crate::machine::{Machine, SendTarget};
use crate::observer::Observer;

/// Upper bound on events parked by `onUnhandled: "defer"` (oldest are evicted).
pub const MAX_DEFERRED: usize = 1000;

/// Outcome of one `send` (or `start` / `tick`).
#[derive(Debug, Clone)]
pub struct Receipt {
    leaves: smallvec::SmallVec<[StateId; 4]>,
    machine: Machine,
    /// Whether the delivered event changed configuration or context.
    pub changed: bool,
    /// Whether no transition matched the delivered event.
    pub unhandled: bool,
    /// User actions executed while processing the event and its consequences, in order
    /// (inline, without allocating, for up to two actions).
    pub actions: crate::engine::ActionList,
    /// Status after processing.
    pub status: Status,
    /// A failure that did not undo the step (see [`StepReport::soft_error`]): the event was
    /// processed, but an addressed event it sent reached no live actor.
    pub error: Option<Arc<Error>>,
}

impl Receipt {
    /// Active leaf state ids after processing, sorted, as `machine.a.b` strings.
    #[must_use]
    pub fn state_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> =
            self.leaves.iter().map(|&s| self.machine.node(s).id.to_string()).collect();
        ids.sort();
        ids
    }

    /// Active leaf states after processing, in document order.
    #[must_use]
    pub fn leaves(&self) -> &[StateId] {
        &self.leaves
    }

    /// `true` when the state with this full id (or custom `#id`) is active or an ancestor of
    /// an active leaf.
    #[must_use]
    pub fn matches(&self, state_id: &str) -> bool {
        self.machine.state_by_id(state_id).is_some_and(|id| {
            self.leaves.iter().any(|&leaf| self.machine.is_self_or_descendant(leaf, id))
        })
    }

    /// Assemble a receipt from its parts. Used by other interpreter implementations.
    #[doc(hidden)]
    #[must_use]
    pub fn from_parts(
        machine: Machine,
        leaves: smallvec::SmallVec<[StateId; 4]>,
        report: &StepReport,
        status: Status,
    ) -> Self {
        Self {
            leaves,
            machine,
            changed: report.changed,
            unhandled: report.unhandled,
            actions: report.actions.clone(),
            status,
            error: report.soft_error.clone(),
        }
    }

    /// Executed user action names as plain strings (convenience for tests).
    #[must_use]
    pub fn action_names(&self) -> Vec<String> {
        self.actions.iter().map(std::string::ToString::to_string).collect()
    }
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
    logic: MachineLogic<C>,
    clock: ClockHandle,
    input: Value,
    state: Option<ActiveState<C>>,
    queue: VecDeque<Event>,
    timers: BinaryHeap<Timer>,
    cancelled: HashSet<u64>,
    next_seq: u64,
    processing: bool,
    emitted: Vec<Value>,
    logs: Vec<(Option<String>, Value)>,
    effects: Vec<Effect>,
    observers: Vec<Arc<dyn Observer<C>>>,
    deferred: VecDeque<Event>,
    /// Soft error recorded while executing the current step's effects.
    soft_error: Option<Arc<Error>>,
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
        Self::with_clock_handle(machine, logic, ClockHandle::Shared(shared_real_clock()))
    }

    /// Create an interpreter driven by the given clock.
    #[must_use]
    pub fn with_clock(machine: Machine, logic: MachineLogic<C>, clock: Arc<dyn Clock>) -> Self {
        Self::with_clock_handle(machine, logic, ClockHandle::Custom(clock))
    }

    fn with_clock_handle(machine: Machine, logic: MachineLogic<C>, clock: ClockHandle) -> Self {
        Self {
            machine,
            logic,
            clock,
            input: Value::Null,
            state: None,
            queue: VecDeque::new(),
            timers: BinaryHeap::new(),
            cancelled: HashSet::default(),
            next_seq: 0,
            processing: false,
            emitted: Vec::new(),
            logs: Vec::new(),
            effects: Vec::new(),
            observers: Vec::new(),
            deferred: VecDeque::new(),
            soft_error: None,
        }
    }

    /// Set the actor input passed to the context factory.
    #[must_use]
    pub fn with_input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    /// Attach an observer (plugin). Observers are notified in attachment order.
    pub fn observe(&mut self, observer: Arc<dyn Observer<C>>) -> &mut Self {
        self.observers.push(observer);
        self
    }

    /// Attach a plugin (convenience alias for [`Self::observe`]).
    pub fn plugin(&mut self, plugin: Arc<dyn Observer<C>>) -> &mut Self {
        self.observe(plugin)
    }

    /// Events parked by `onUnhandled: "defer"`, oldest first.
    pub fn deferred_events(&self) -> impl Iterator<Item = &Event> + '_ {
        self.deferred.iter()
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
        let mut effects = std::mem::take(&mut self.effects);
        effects.clear();
        let started = engine::start_into(
            &self.machine,
            &self.logic,
            &self.input,
            false,
            &mut effects,
            &mut self.state,
        );
        let mut report = match started {
            Ok(report) => report,
            Err(err) => {
                effects.clear();
                self.effects = effects;
                return Err(err);
            }
        };
        self.apply_effects(&mut effects);
        self.effects = effects;
        report.soft_error = self.soft_error.take();
        let rest = self.drain()?;
        report.actions.extend(rest.actions);
        report.changed = true;
        if let Some(state) = self.state.as_ref() {
            for obs in &self.observers {
                obs.on_start(&self.machine, state, &report);
            }
        }
        Ok(self.receipt(report))
    }

    /// Deliver an event and process it, plus everything it triggers (timers already due,
    /// synchronous service completions), to completion.
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] before `start`; [`Error::UnknownEvent`] under `strict` for
    /// an undeclared event; logic errors according to the machine's policies.
    pub fn send(&mut self, event: impl Into<Event>) -> Result<Receipt> {
        let event = event.into();
        if event.ty.is_empty() {
            return Err(Error::InvalidEvent("event type must be a non-empty string".into()));
        }
        let state = self.state.as_ref().ok_or_else(|| Error::InvalidLifecycle {
            status: "uninitialized",
            message: "send before start".into(),
        })?;
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
        let _ = state;
        if self.processing {
            // Re-entrant send from inside an action: queued, processed by the outer loop.
            self.queue.push_back(event);
            return Ok(self.receipt(StepReport::default()));
        }
        // Nothing queued ahead of it (pending events of a restored snapshot would be): the
        // event is processed first, exactly as if it had gone through the queue.
        let mut report = StepReport::default();
        if self.queue.is_empty() {
            self.drain_into(Some(&event), &mut report)?;
        } else {
            self.queue.push_back(event);
            self.drain_into(None, &mut report)?;
        }
        Ok(self.receipt(report))
    }

    /// Deliver an event and process it to completion, returning whether the configuration or
    /// context changed, without allocating an owned [`Receipt`].
    ///
    /// This is the ultra-fast execution path for high-throughput loops (>12M ops/s), avoiding
    /// the atomic reference-count increment on the machine's `Arc` and leaf state collection.
    ///
    /// # Errors
    /// Same as [`send`](Self::send).
    /// Send an event and advance the interpreter, returning whether the active configuration
    /// changed (`Ok(true)`) or remained unchanged (`Ok(false)`).
    ///
    /// # Fast-Path Steady-State Bypass (69.8 ns p50)
    /// In steady-state operation—where no internal events are queued, no timers are pending,
    /// no observers are attached, and no deferred events are parked—this method bypasses the
    /// [`Receipt`] allocation, leaf calculation, and `Arc<Machine>` clone overhead of [`send`](Self::send).
    /// It directly drives the sans-I/O step engine via [`engine::step_into`].
    ///
    /// If user actions produce effects (such as `raise`, `assign`, or actor spawns) or the event
    /// is unhandled and requires policy escalation, execution transparently falls back to the full
    /// handler pipeline ([`apply_effects`](Self::apply_effects) and [`after_step`](Self::after_step)).
    ///
    /// # Errors
    /// Returns [`Error::InvalidEvent`] if the event type is empty, [`Error::InvalidLifecycle`] if
    /// called before [`start`](Self::start), [`Error::UnknownEvent`] if running under strict mode and
    /// the event is undeclared, or propagates action/guard failures.
    #[inline]
    pub fn send_fast(&mut self, event: impl Into<Event>) -> Result<bool> {
        let event = event.into();
        if event.ty.is_empty() {
            return Err(Error::InvalidEvent("event type must be a non-empty string".into()));
        }
        let Some(state) = self.state.as_mut() else {
            return Err(Error::InvalidLifecycle {
                status: "uninitialized",
                message: "send before start".into(),
            });
        };
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
        if self.processing {
            self.queue.push_back(event);
            return Ok(false);
        }

        // Fast steady-state path: no queued events, no timers, no observers, no deferred events.
        // Direct jump into the pure sans-I/O step engine.
        if self.queue.is_empty()
            && self.timers.is_empty()
            && self.observers.is_empty()
            && self.deferred.is_empty()
            && state.status == Status::Running
        {
            self.effects.clear();
            let mut report = StepReport::default();
            engine::step_into(
                &self.machine,
                &self.logic,
                state,
                &event,
                &mut self.effects,
                false,
                &mut report,
            )?;
            // If no effects were produced and the transition was either handled or unhandled
            // with `Ignore` policy, complete immediately without observer or queue passes.
            if self.effects.is_empty()
                && (!report.unhandled
                    || self.machine.policies().on_unhandled == UnhandledPolicy::Ignore)
            {
                return Ok(report.changed);
            }
            // Side effects or policy escalation: route through full apply_effects and after_step.
            if !self.effects.is_empty() {
                let mut effects = std::mem::take(&mut self.effects);
                self.apply_effects(&mut effects);
                self.effects = effects;
            }
            self.after_step(&event, &report);
            if !self.queue.is_empty() || !self.timers.is_empty() {
                let mut rest = StepReport::default();
                self.drain_into(None, &mut rest)?;
                report.changed |= rest.changed;
            }
            return Ok(report.changed);
        }

        let mut report = StepReport::default();
        if self.queue.is_empty() {
            self.drain_into(Some(&event), &mut report)?;
        } else {
            self.queue.push_back(event);
            self.drain_into(None, &mut report)?;
        }
        Ok(report.changed)
    }

    /// Fast-path send for event names without payload: copies inline up to 23 bytes without
    /// heap allocation and advances the interpreter via [`send_fast`](Self::send_fast).
    ///
    /// Ideal for plain signal events such as `"TOGGLE"`, `"NEXT"`, or `"SUBMIT"`.
    ///
    /// # Errors
    /// Invalid events, lifecycle errors, unknown events in strict mode, or action failures.
    #[inline]
    pub fn send_str(&mut self, event_type: &str) -> Result<bool> {
        let ty = if event_type.len() <= SMOL_INLINE_CAP {
            smol_str::SmolStr::new_inline(event_type)
        } else {
            smol_str::SmolStr::new(event_type)
        };
        self.send_fast(Event { ty, payload: Value::Null, origin: EventOrigin::External })
    }

    /// Fire every timer that is due according to the clock, in due order. The receipt covers
    /// everything processed.
    ///
    /// # Errors
    /// Logic errors raised while processing the timer events.
    pub fn tick(&mut self) -> Result<Receipt> {
        if self.state.is_none() || self.processing {
            return Ok(self.receipt(StepReport::default()));
        }
        let report = self.drain()?;
        Ok(self.receipt(report))
    }

    /// Advance a [`SimulatedClock`] and fire what became due. Pass the same clock the
    /// interpreter was constructed with.
    ///
    /// # Errors
    /// Logic errors raised while processing the timer events.
    pub fn advance(&mut self, clock: &SimulatedClock, ms: f64) -> Result<Receipt> {
        clock.try_increment(ms)?;
        self.tick()
    }

    /// Stop the machine: pending events and timers are dropped.
    pub fn stop(&mut self) {
        if let Some(state) = self.state.as_mut().filter(|s| s.status == Status::Running) {
            state.status = Status::Stopped;
        }
        for event in self.queue.drain(..) {
            for obs in &self.observers {
                obs.on_event_dropped(&self.machine, &event, "stopped");
            }
        }
        self.deferred.clear();
        self.timers.clear();
        self.cancelled.clear();
        if let Some(state) = self.state.as_ref() {
            for obs in &self.observers {
                obs.on_stop(&self.machine, state);
            }
        }
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

    fn receipt(&self, report: StepReport) -> Receipt {
        Receipt {
            leaves: self.state.as_ref().map(|s| s.leaves(&self.machine)).unwrap_or_default(),
            machine: self.machine.clone(),
            changed: report.changed,
            unhandled: report.unhandled,
            actions: report.actions,
            status: self.status(),
            error: report.soft_error,
        }
    }

    /// Process queued events and due timers until nothing is left. Returns the merged report
    /// of everything processed.
    fn drain(&mut self) -> Result<StepReport> {
        let mut report = StepReport::default();
        self.drain_into(None, &mut report)?;
        Ok(report)
    }

    /// [`drain`](Self::drain), processing `first` before anything queued and merging into
    /// `merged` (a fresh report).
    fn drain_into(&mut self, first: Option<&Event>, merged: &mut StepReport) -> Result<()> {
        self.processing = true;
        let result = self.drain_inner(first, merged);
        self.processing = false;
        result
    }

    fn drain_inner(&mut self, first: Option<&Event>, merged: &mut StepReport) -> Result<()> {
        // The first processed event's report is the merged report's base.
        let mut fresh = true;
        if let Some(event) = first {
            self.process(event, merged, &mut fresh)?;
        }
        while self.state.is_some()
            && let Some(event) = self.next_event()
        {
            self.process(&event, merged, &mut fresh)?;
        }
        Ok(())
    }

    /// Process one drained event into `merged`; `fresh` until the first one is in.
    fn process(&mut self, event: &Event, merged: &mut StepReport, fresh: &mut bool) -> Result<()> {
        let Some(state) = self.state.as_ref() else { return Ok(()) };
        if state.status != Status::Running {
            if *fresh {
                merged.unhandled = true;
                *fresh = false;
            }
            for obs in &self.observers {
                obs.on_event_dropped(&self.machine, event, "not_running");
            }
        } else if *fresh {
            self.step_event(event, merged)?;
            *fresh = false;
        } else {
            let mut report = StepReport::default();
            self.step_event(event, &mut report)?;
            merge_report(merged, report);
        }
        Ok(())
    }

    /// The next queued event, else the next due timer's.
    fn next_event(&mut self) -> Option<Event> {
        self.queue.pop_front().or_else(|| self.pop_due_timer())
    }

    /// Run one event through the engine, notify the observers and execute the effects.
    fn step_event(&mut self, event: &Event, report: &mut StepReport) -> Result<()> {
        let policy = self.machine.policies().on_unhandled;
        let observed = !self.observers.is_empty()
            || (policy != UnhandledPolicy::Ignore && event.origin == EventOrigin::External);
        self.effects.clear();
        let Some(state) = self.state.as_mut() else { return Ok(()) };
        let stepped = engine::step_into(
            &self.machine,
            &self.logic,
            state,
            event,
            &mut self.effects,
            false,
            report,
        );
        if let Err(err) = stepped {
            for obs in &self.observers {
                obs.on_error(&self.machine, &err);
            }
            return Err(err);
        }
        if observed {
            self.after_step(event, report);
        }
        // Observers saw the report before the effects ran (as before); a soft error from
        // executing them (an unresolved `sendTo`) reaches them as `on_event_dropped` and
        // the caller through the receipt. Most steps request nothing.
        if !self.effects.is_empty() {
            let mut effects = std::mem::take(&mut self.effects);
            self.apply_effects(&mut effects);
            self.effects = effects;
            report.soft_error = self.soft_error.take();
        }
        Ok(())
    }

    /// Observer notification and the `onUnhandled` policy for one processed event.
    fn after_step(&mut self, event: &Event, report: &StepReport) {
        let Some(state) = self.state.as_ref() else { return };
        for obs in &self.observers {
            obs.on_event(&self.machine, event, report, state);
        }
        if report.unhandled && event.origin == EventOrigin::External {
            match self.machine.policies().on_unhandled {
                UnhandledPolicy::Ignore => {}
                UnhandledPolicy::Defer => {
                    if self.deferred.len() >= MAX_DEFERRED
                        && let Some(dropped) = self.deferred.pop_front()
                    {
                        for obs in &self.observers {
                            obs.on_event_dropped(&self.machine, &dropped, "deferred_overflow");
                        }
                    }
                    for obs in &self.observers {
                        obs.on_event_dropped(&self.machine, event, "deferred");
                    }
                    self.deferred.push_back(event.clone());
                }
                UnhandledPolicy::Error => {
                    if let Some(state) = self.state.as_mut() {
                        state.status = Status::Error;
                        state.error = Some(format!("unhandled event '{}'", event.ty));
                    }
                    self.queue.clear();
                }
            }
        } else if !report.transitions.is_empty() && !self.deferred.is_empty() {
            // The configuration changed: give parked events another chance, in order, before
            // anything newer.
            let mut replay: Vec<Event> = self.deferred.drain(..).collect();
            while let Some(ev) = replay.pop() {
                self.queue.push_front(ev);
            }
        }
    }

    fn pop_due_timer(&mut self) -> Option<Event> {
        if self.timers.is_empty() {
            return None;
        }
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

    fn apply_effects(&mut self, effects: &mut Vec<Effect>) {
        // The common step only exits states: with no timer armed, cancelling is a no-op.
        if self.timers.is_empty() && effects.iter().all(|e| matches!(e, Effect::CancelTimers(_))) {
            effects.clear();
            return;
        }
        for effect in effects.drain(..) {
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
                // A delayed self-send is a timer of this machine.
                Effect::SendTo { to: SendTarget::SelfActor, event, delay_ms, id } => {
                    let event = Event { origin: EventOrigin::Internal, ..event };
                    self.arm(delay_ms.unwrap_or(0.0), event, None, id);
                }
                // No actor system: no child, parent or `systemId` actor is ever live here.
                Effect::SendTo { to, event, .. } => {
                    let (action, target) = match to {
                        SendTarget::Parent => ("sendParent", "<no parent>".to_owned()),
                        SendTarget::Named(name) => ("sendTo", name),
                        SendTarget::SelfActor => ("sendTo", "self".to_owned()),
                    };
                    self.unresolved(action, target, &event);
                }
                Effect::Spawn { id, .. } => {
                    tracing::warn!(target: "xstate", id = %id, "spawnChild is not supported by SyncInterpreter yet; dropped");
                }
                Effect::Escalate(err) => {
                    let event = Event::error_actor(self.machine.id(), err);
                    self.unresolved("escalate", "<no parent>".to_owned(), &event);
                }
            }
        }
    }

    /// An addressed event reached no live actor: dropped, reported to the observers
    /// (`unresolved_target`) and recorded as the step's soft error.
    fn unresolved(&mut self, action: &'static str, target: String, event: &Event) {
        tracing::warn!(target: "xstate", action, %target, event = %event.ty, "target did not resolve to a live actor; event dropped");
        for obs in &self.observers {
            obs.on_event_dropped(&self.machine, event, "unresolved_target");
        }
        if self.soft_error.is_none() {
            let error = Error::UnresolvedTarget { action, target, event: event.ty.to_string() };
            self.soft_error = Some(Arc::new(error));
        }
    }

    fn run_service(&mut self, id: &str, src: &str, input: &Value) {
        let Some(state) = self.state.as_ref() else { return };
        let outcome = match self.logic.get_service(src) {
            Ok(ServiceImpl::Sync(f)) => f(&state.context, input),
            Ok(ServiceImpl::Async(_) | ServiceImpl::Machine(_)) => Err(Error::InvalidLifecycle {
                status: self.status().as_str(),
                message: format!(
                    "service '{src}' is asynchronous or a child machine; use the tokio interpreter"
                ),
            }),
            Err(err) => Err(err),
        };
        let event = match outcome {
            Ok(output) => Event::done_actor(id, output),
            Err(err) => Event::error_actor(id, Value::String(err.to_string())),
        };
        self.queue.push_back(event);
    }
}

/// Fold the report of a later event of the same `send` into the first one's.
fn merge_report(merged: &mut StepReport, report: StepReport) {
    merged.changed |= report.changed;
    merged.actions.extend(report.actions);
    merged.transitions.extend(report.transitions);
    merged.microsteps += report.microsteps;
    if report.failed.is_some() {
        merged.failed = report.failed;
    }
    if merged.soft_error.is_none() {
        merged.soft_error = report.soft_error;
    }
}

/// The process-wide monotonic clock shared by interpreters built without an explicit clock.
/// Only differences of its readings matter (timer due times), so sharing one origin is
/// equivalent to a clock per interpreter, and saves an allocation and a clock read per
/// interpreter.
fn shared_real_clock() -> &'static RealClock {
    static CLOCK: OnceLock<RealClock> = OnceLock::new();
    CLOCK.get_or_init(RealClock::new)
}

/// The clock of an interpreter: the shared real clock (borrowed, so creating and dropping an
/// interpreter touches no reference count) or one the application supplied.
enum ClockHandle {
    Shared(&'static RealClock),
    Custom(Arc<dyn Clock>),
}

impl std::ops::Deref for ClockHandle {
    type Target = dyn Clock;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Shared(clock) => *clock,
            Self::Custom(clock) => &**clock,
        }
    }
}

mod restore;

pub use restore::{RestartTimers, RestoreOptions};

impl From<&str> for Event {
    #[inline]
    fn from(ty: &str) -> Self {
        // Short names are copied inline straight away (`SmolStr::new` first looks for its
        // whitespace-only representation).
        if ty.len() <= SMOL_INLINE_CAP {
            Event::new(smol_str::SmolStr::new_inline(ty))
        } else {
            Event::new(ty)
        }
    }
}

impl From<String> for Event {
    fn from(ty: String) -> Self {
        Event::new(ty)
    }
}

/// `smol_str`'s inline capacity: names up to this length never allocate.
const SMOL_INLINE_CAP: usize = 23;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_fast_updates_state_and_reports_change() {
        let machine = Machine::from_json(
            r#"{
            "id": "toggle", "initial": "a",
            "states": {
                "a": { "on": { "TOGGLE": "b" } },
                "b": { "on": { "TOGGLE": "a" } }
            }
        }"#,
        )
        .unwrap();
        let mut i = SyncInterpreter::new(machine, MachineLogic::dynamic());
        assert!(i.send_fast("TOGGLE").is_err()); // before start
        i.start().unwrap();
        assert_eq!(i.state_ids(), ["toggle.a"]);

        let changed = i.send_fast("TOGGLE").unwrap();
        assert!(changed);
        assert_eq!(i.state_ids(), ["toggle.b"]);

        let changed = i.send_fast("UNKNOWN").unwrap();
        assert!(!changed); // unhandled
        assert_eq!(i.state_ids(), ["toggle.b"]);

        let changed = i.send_fast("TOGGLE").unwrap();
        assert!(changed);
        assert_eq!(i.state_ids(), ["toggle.a"]);

        let changed = i.send_str("TOGGLE").unwrap();
        assert!(changed);
        assert_eq!(i.state_ids(), ["toggle.b"]);

        let changed = i.send_str("UNKNOWN").unwrap();
        assert!(!changed);
        assert_eq!(i.state_ids(), ["toggle.b"]);
    }
}
