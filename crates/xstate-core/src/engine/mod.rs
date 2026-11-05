//! The sans-I/O step engine (ADR-0004).
//!
//! [`start`] and [`step`] implement the W3C SCXML Appendix D algorithm as realised by
//! XState v5 — selection, conflict resolution, exit / transition / entry ordering, history,
//! eventless settling and `done.state` propagation — over an [`ActiveState`]. They perform no
//! I/O: timers, invoked services, child actors and emitted events come back to the caller as
//! [`Effect`]s, which the synchronous interpreter and the tokio interpreter execute.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

use crate::context::Context;
use crate::error::Result;
use crate::event::Event;
use crate::ids::{StateId, TransitionId};
use crate::logic::MachineLogic;
use crate::machine::{Delay, InvokeDef, Machine, SendTarget};

mod actions;
mod algorithm;
pub(crate) mod bits;
mod select;
pub(crate) mod sets;

use bits::StateBits;

/// Lifecycle status of a running machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Accepting events.
    Running,
    /// Reached a top-level final state.
    Done,
    /// Failed (unhandled service error, runaway chain, failed action under `fail` policy).
    Error,
    /// Stopped by the application.
    Stopped,
}

impl Status {
    /// Lower-case name as used in snapshots and receipts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Stopped => "stopped",
        }
    }
}

/// Recorded history: exited state → every descendant that was active, in document order.
pub(crate) type HistoryMap = HashMap<StateId, Vec<StateId>>;

/// Mutable runtime state of one machine instance.
#[derive(Debug, Clone)]
pub struct ActiveState<C> {
    pub(crate) configuration: StateBits,
    pub(crate) context: C,
    pub(crate) history: HistoryMap,
    pub(crate) status: Status,
    pub(crate) output: Value,
    pub(crate) error: Option<String>,
    pub(crate) internal_queue: VecDeque<Event>,
    /// Bumped whenever a microstep produces a new snapshot in XState's sense: a configuration
    /// change, a recorded history value, an `assign`, a started or stopped actor. The macrostep
    /// re-selects eventless transitions only after such a microstep (or an internal event).
    pub(crate) mutations: u64,
    /// Count of state entries so far; stamps `after` timers for durable deadlines.
    pub(crate) entry_seq: u64,
    /// Per state (indexed by [`StateId::index`]): the `entry_seq` of its latest entry, `0` for
    /// a restored configuration. XState keeps its active states as a list (`snapshot._nodes`)
    /// to which a microstep appends what it enters, and `selectEventlessTransitions` walks the
    /// atomic states in that order; sorting the active leaves by this stamp reproduces it.
    /// Only eventless selection reads it, so it is empty (and not kept) for a machine without
    /// `always` transitions.
    pub(crate) entered_at: Vec<u64>,
}

impl<C> ActiveState<C> {
    /// Current status.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status
    }

    /// Extended state.
    #[must_use]
    pub fn context(&self) -> &C {
        &self.context
    }

    /// Mutable access to the extended state (for tests and adapters; actions should use the
    /// action scope).
    pub fn context_mut(&mut self) -> &mut C {
        &mut self.context
    }

    /// Machine output once `Done`, `null` otherwise.
    #[must_use]
    pub fn output(&self) -> &Value {
        &self.output
    }

    /// Error message once `Error`.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Every active state (ancestors included) in document order.
    pub fn configuration(&self) -> impl Iterator<Item = StateId> + '_ {
        self.configuration.states()
    }

    /// `true` when `state` is active.
    #[must_use]
    pub fn is_active(&self, state: StateId) -> bool {
        self.configuration.contains(state.index())
    }

    /// Recorded history: exited state → every descendant that was active at exit time.
    #[must_use]
    pub fn history(&self) -> &HistoryMap {
        &self.history
    }

    /// Active leaf states in document order.
    #[must_use]
    pub fn leaves(&self, machine: &Machine) -> smallvec::SmallVec<[StateId; 4]> {
        let mut out = smallvec::SmallVec::new();
        self.configuration.states_masked_into(machine.leaf_mask(), &mut out);
        out
    }

    /// Active leaf state ids, sorted, in the format `machine.a.b`.
    #[must_use]
    pub fn leaf_ids(&self, machine: &Machine) -> Vec<String> {
        let mut ids: Vec<String> =
            self.leaves(machine).into_iter().map(|s| machine.node(s).id.to_string()).collect();
        ids.sort();
        ids
    }

    /// XState-style hierarchical state value (`"idle"`, `{"a": {"b": "c"}}`, `{}` when done
    /// at the root).
    #[must_use]
    pub fn value(&self, machine: &Machine) -> Value {
        crate::snapshot::state_value(machine, &|s| self.is_active(s), StateId::ROOT)
    }

    /// Tags of every active state, deduplicated and sorted.
    #[must_use]
    pub fn tags(&self, machine: &Machine) -> Vec<String> {
        let mut tags: Vec<String> =
            self.configuration().flat_map(|s| machine.node(s).tags.iter().cloned()).collect();
        tags.sort();
        tags.dedup();
        tags
    }
}

impl<C: Context> ActiveState<C> {
    /// Mark the machine as stopped by the application (no-op unless running).
    pub fn stop(&mut self) {
        if self.status == Status::Running {
            self.status = Status::Stopped;
            self.internal_queue.clear();
        }
    }

    /// Rebuild runtime state from snapshot parts. The configuration must already be validated.
    pub(crate) fn restore(
        machine: &Machine,
        configuration: &[StateId],
        context: C,
        history: HistoryMap,
        snapshot: &crate::snapshot::Snapshot,
    ) -> Self {
        let mut bits = StateBits::with_capacity(machine.states().len());
        for &s in configuration {
            bits.set(s.index(), true);
        }
        let entry_seq = snapshot.deadlines.iter().map(|d| d.entry_seq).max().unwrap_or(0);
        Self {
            configuration: bits,
            context,
            history,
            status: snapshot.status,
            output: snapshot.output.clone(),
            error: snapshot.error.clone(),
            internal_queue: VecDeque::new(),
            mutations: 0,
            entry_seq,
            // XState's `restoreSnapshot` rebuilds its state list from the state value: the
            // leaves in document order, which is what equal stamps sort as.
            entered_at: entry_stamps(machine),
        }
    }
}

/// Fresh per-state entry stamps: one per state when eventless selection needs them.
fn entry_stamps(machine: &Machine) -> Vec<u64> {
    if machine.has_always() { vec![0; machine.states().len()] } else { Vec::new() }
}

/// A side effect requested by the engine, to be executed by the driver after the step.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Effect {
    /// Arm the `after` timer of `state`; deliver `event` to the machine after `delay_ms`.
    ArmTimer {
        /// Owning state.
        state: StateId,
        /// Index into the state's `after` list.
        index: usize,
        /// Sequence number of the state entry that armed the timer (durable deadlines).
        entry_seq: u64,
        /// Delay in milliseconds (already resolved).
        delay_ms: f64,
        /// The `xstate.after.*` event to deliver.
        event: Event,
    },
    /// Cancel every pending `after` timer of `state` (it was exited).
    CancelTimers(StateId),
    /// Start an invoked service.
    StartInvoke {
        /// Owning state.
        state: StateId,
        /// Service definition (id, src, systemId).
        invoke: InvokeDef,
        /// Resolved input.
        input: Value,
    },
    /// Stop an invoked service or child actor by id.
    StopActor(String),
    /// Deliver an event to this machine after a delay (`raise` with `delay`).
    DelayedRaise {
        /// The event.
        event: Event,
        /// Delay in milliseconds.
        delay_ms: f64,
        /// Cancellation id.
        id: Option<String>,
    },
    /// Cancel a delayed send by id.
    CancelSend(String),
    /// Send an event to another actor.
    SendTo {
        /// Recipient.
        to: SendTarget,
        /// The event.
        event: Event,
        /// Delay in milliseconds, if any.
        delay_ms: Option<f64>,
        /// Cancellation id.
        id: Option<String>,
    },
    /// Spawn a child actor.
    Spawn {
        /// Logic name.
        src: String,
        /// Child id.
        id: String,
        /// `systemId`.
        system_id: Option<String>,
        /// Input.
        input: Value,
    },
    /// Emit to external listeners.
    Emit(Value),
    /// `log` built-in.
    Log {
        /// Expression value.
        expr: Value,
        /// Label.
        label: Option<String>,
    },
    /// Escalate an error to the parent actor.
    Escalate(Value),
    /// Internal: queue an event for this macrostep. Drained by the engine, never surfaced.
    Raise(Event),
}

/// What happened during one [`step`] (or [`start`]).
#[derive(Debug, Clone, Default)]
pub struct StepReport {
    /// Whether the configuration or context changed.
    pub changed: bool,
    /// Transitions taken, in execution order (across all microsteps).
    pub transitions: smallvec::SmallVec<[TransitionId; 4]>,
    /// User action names executed, in order.
    pub actions: Vec<std::sync::Arc<str>>,
    /// Number of microsteps executed.
    pub microsteps: u32,
    /// `true` when no transition matched the delivered event.
    pub unhandled: bool,
    /// Set when an action failed and the step was rolled back (`actionErrorPolicy: rollback`).
    pub failed: Option<String>,
    /// Pure mode only: the user actions that would have run, with their params.
    pub planned: Vec<crate::pure::PlannedAction>,
    /// A failure that did not undo the step: the transition committed, but something it asked
    /// for did not happen -- an addressed event (`sendTo`, `forwardTo`, `sendParent`,
    /// `escalate`) that reached no live actor ([`Error::UnresolvedTarget`]). Set by the
    /// interpreter once it has executed the effects, so it reaches the receipt; observers are
    /// told through `on_event_dropped("unresolved_target")` instead.
    ///
    /// [`Error::UnresolvedTarget`]: crate::Error::UnresolvedTarget
    pub soft_error: Option<std::sync::Arc<crate::Error>>,
}

/// Everything the algorithm needs for one macrostep.
pub(crate) struct Ctx<'a, C> {
    pub machine: &'a Machine,
    pub logic: &'a MachineLogic<C>,
    pub state: &'a mut ActiveState<C>,
    pub effects: &'a mut Vec<Effect>,
    pub report: &'a mut StepReport,
    /// Pure mode: user actions are planned, not executed (see [`crate::pure`]).
    pub pure: bool,
}

/// Enter the initial configuration, running entry actions and settling eventless
/// transitions. Returns the state plus the effects to execute.
///
/// # Errors
/// Propagates logic errors (missing implementations, failing actions under the `fail`
/// policy) and context construction errors.
pub fn start<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    input: &Value,
) -> Result<(ActiveState<C>, Vec<Effect>, StepReport)> {
    start_with(machine, logic, input, false)
}

pub(crate) fn start_with<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    input: &Value,
    pure: bool,
) -> Result<(ActiveState<C>, Vec<Effect>, StepReport)> {
    let mut effects = Vec::new();
    let (state, report) = start_into(machine, logic, input, pure, &mut effects)?;
    Ok((state, effects, report))
}

/// [`start_with`] appending the effects to `effects` (a buffer reused across steps).
pub(crate) fn start_into<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    input: &Value,
    pure: bool,
    effects: &mut Vec<Effect>,
) -> Result<(ActiveState<C>, StepReport)> {
    let first = effects.len();
    let context = C::from_initial(machine.initial_context(), input)?;
    let mut state = ActiveState {
        configuration: StateBits::with_capacity(machine.states().len()),
        context,
        history: HashMap::new(),
        status: Status::Running,
        output: Value::Null,
        error: None,
        internal_queue: VecDeque::new(),
        mutations: 0,
        entry_seq: 0,
        entered_at: entry_stamps(machine),
    };
    let mut report = StepReport::default();
    {
        let mut cx = Ctx { machine, logic, state: &mut state, effects, report: &mut report, pure };
        algorithm::start(&mut cx)?;
    }
    finalize_effects(effects, first);
    Ok((state, report))
}

/// Process one external event to completion (a macrostep).
///
/// Effects are appended to `effects`. A machine that is not [`Status::Running`] ignores the
/// event and reports it as unhandled.
///
/// # Errors
/// Propagates logic errors according to the machine's error policies.
#[allow(clippy::needless_pass_by_value)] // public signature; the step borrows the event
pub fn step<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    state: &mut ActiveState<C>,
    event: Event,
    effects: &mut Vec<Effect>,
) -> Result<StepReport> {
    step_with(machine, logic, state, &event, effects, false)
}

/// [`step`] for a borrowed event; `pure` plans user actions instead of running them.
pub(crate) fn step_with<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    state: &mut ActiveState<C>,
    event: &Event,
    effects: &mut Vec<Effect>,
    pure: bool,
) -> Result<StepReport> {
    let mut report = StepReport::default();
    step_into(machine, logic, state, event, effects, pure, &mut report)?;
    Ok(report)
}

/// [`step_with`] writing into `report`, which must be a fresh (default) report.
pub(crate) fn step_into<C: Context>(
    machine: &Machine,
    logic: &MachineLogic<C>,
    state: &mut ActiveState<C>,
    event: &Event,
    effects: &mut Vec<Effect>,
    pure: bool,
    report: &mut StepReport,
) -> Result<()> {
    if state.status != Status::Running {
        report.unhandled = true;
        return Ok(());
    }
    let first = effects.len();
    let rollback =
        machine.policies().action_error_policy == crate::config::ActionErrorPolicy::Rollback;
    let checkpoint = rollback.then(|| state.clone());
    let outcome = {
        let mut cx = Ctx { machine, logic, state, effects, report: &mut *report, pure };
        algorithm::macrostep(&mut cx, event)
    };
    match outcome {
        Ok(()) => {}
        Err(err @ crate::Error::TransitionFailed { .. }) if rollback => {
            // Restore the pre-step state; external side effects are not undone.
            if let Some(saved) = checkpoint {
                *state = saved;
            }
            effects.truncate(first);
            *report = StepReport { failed: Some(err.to_string()), ..StepReport::default() };
            return Ok(());
        }
        Err(err) => {
            // XState: a failing action is unrecoverable for the actor.
            state.status = Status::Error;
            state.error = Some(err.to_string());
            state.internal_queue.clear();
            effects.truncate(first);
            return Err(err);
        }
    }
    finalize_effects(effects, first);
    Ok(())
}

/// Drop start/stop pairs for actors that were started and stopped within the same step, and
/// strip the engine-internal `Raise` markers (they were consumed by the macrostep). Only the
/// tail of `effects` starting at `first` belongs to this step.
fn finalize_effects(effects: &mut Vec<Effect>, first: usize) {
    let tail = &effects[first..];
    if !tail.iter().any(|e| matches!(e, Effect::StartInvoke { .. } | Effect::Raise(_))) {
        return;
    }
    let mut keep = vec![true; tail.len()];
    for (i, e) in tail.iter().enumerate() {
        match e {
            Effect::StartInvoke { invoke, .. } => {
                let stopped = tail[i + 1..]
                    .iter()
                    .position(|later| matches!(later, Effect::StopActor(id) if *id == invoke.id));
                if let Some(j) = stopped {
                    keep[i] = false;
                    keep[i + 1 + j] = false;
                }
            }
            Effect::Raise(_) => keep[i] = false,
            _ => {}
        }
    }
    let mut index = 0;
    effects.retain(|_| {
        let kept = index < first || keep[index - first];
        index += 1;
        kept
    });
}

/// Resolve a configured delay to milliseconds.
pub(crate) fn resolve_delay<C: Context>(
    logic: &MachineLogic<C>,
    delay: &Delay,
    context: &C,
    event: &Event,
) -> Result<f64> {
    match delay {
        Delay::Fixed(ms) => Ok(*ms),
        Delay::Named(name) => match logic.get_delay(name)? {
            crate::logic::DelayImpl::Fixed(ms) => Ok(*ms),
            crate::logic::DelayImpl::Dynamic(f) => f(context, event),
        },
    }
}

/// A delay as usable milliseconds for `after`, a delayed `raise` and a delayed `sendTo`
/// alike, or `None` when it cannot be used: the delay function failed, or the value is NaN or
/// infinite. The failure is logged and only that one timer is disabled; a misbehaving delay
/// never fails the step (it runs during state entry). A negative delay fires at once.
///
/// # Errors
/// Only [`crate::Error::ImplementationMissing`] for an unregistered delay name (a broken
/// logic registry, normally refused by `MachineLogic::validate` before the machine starts).
pub(crate) fn usable_delay<C: Context>(
    cx: &Ctx<'_, C>,
    delay: &Delay,
    event: &Event,
) -> Result<Option<f64>> {
    match resolve_delay(cx.logic, delay, &cx.state.context, event) {
        Ok(ms) if ms.is_finite() => Ok(Some(ms.max(0.0))),
        Ok(ms) => {
            tracing::error!(target: "xstate", ?delay, value = ms, "delay is not a finite number of milliseconds; ignoring this timer");
            Ok(None)
        }
        Err(err @ crate::Error::ImplementationMissing { .. }) => Err(err),
        Err(err) => {
            tracing::error!(target: "xstate", ?delay, error = %err, "delay function failed; ignoring this timer");
            Ok(None)
        }
    }
}
