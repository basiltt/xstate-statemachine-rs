//! Drive a `TOOL_LOOP` machine to a resting state: [`AgentSession`], [`run_agent`] and
//! [`AgentResult`].
//!
//! A convenience loop, not a second runtime. A session starts (or restores from a
//! [`Snapshot`]) a [`SyncInterpreter`], sends `START` with the prompt when the machine is in
//! `idle`, sends the resume events (an arbitrary event, then the reviewer's decision), and
//! reports where the machine rests. Sync services complete inside the step, so a run returns
//! as soon as the events are processed; an `after` timeout needs the caller to advance the
//! clock ([`SyncInterpreter::advance`] on a [`SimulatedClock`](xstate_core::SimulatedClock)).
//!
//! Python persists through `store=` / `key=`; here the resting [`Snapshot`] is returned
//! (see [`AgentRun`]) and handed back to [`AgentSession::restore`] to resume -- including the
//! `after` escalation deadline of `awaiting_human`.

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_core::{
    Clock, DynContext, Event, Machine, MachineLogic, Observer, RealClock, RestoreOptions, Snapshot,
    Status, SyncInterpreter,
};

use crate::budgets::{ctx_float, ctx_int};
use crate::error::{AgentError, Result};
use crate::logic::AgentLogic;
use crate::tools::approval::decision_event;
use crate::trace::AgentTrace;

/// States in which a run returns because a HUMAN (or another process) must act next. They
/// persist; the machine is not finished.
pub const WAITING_STATES: &[&str] = &["awaiting_human"];

pub use crate::tools::approval::pending_approval;

/// Where the agent came to rest.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentResult {
    /// The leaf state id (`toolLoop.done`), comma-joined when several are active.
    pub final_state: String,
    /// Interpreter status: `done`, `running`, `error`, `stopped`.
    pub status: String,
    /// A copy of the context at rest.
    pub context: Value,
    /// `context["result"]` (validated output or final text).
    pub output: Value,
    /// `context["error"]` (`{"kind", "message"}`) or `null`.
    pub error: Value,
    /// `true` when resting in a durable wait state.
    pub waiting: bool,
}

impl AgentResult {
    /// `{"turns", "input_tokens", "output_tokens", "cost_usd"}`.
    #[must_use]
    pub fn usage(&self) -> Value {
        let c = &self.context;
        json!({
            "turns": ctx_int(c, "turns"),
            "input_tokens": ctx_int(c, "tokens_in"),
            "output_tokens": ctx_int(c, "tokens_out"),
            "cost_usd": ctx_float(c, "cost_usd"),
        })
    }

    /// `true` when the agent ended in error (`context["error"]` is set).
    #[must_use]
    pub fn failed(&self) -> bool {
        !self.error.is_null()
    }
}

/// What to send on one run.
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// Sent as `START {prompt}` when the machine is in `idle`.
    pub prompt: Option<String>,
    /// Any other event to send first.
    pub event: Option<Event>,
    /// Resume a durable wait: `true` sends `HUMAN_APPROVED` naming the pending call ids,
    /// `false` sends `HUMAN_REJECTED`.
    pub approve: Option<bool>,
}

impl RunInput {
    /// A run starting the agent on `prompt`.
    pub fn prompt(prompt: impl Into<String>) -> Self {
        Self { prompt: Some(prompt.into()), ..Self::default() }
    }

    /// A run resuming a wait with the reviewer's decision.
    #[must_use]
    pub fn approve(approve: bool) -> Self {
        Self { approve: Some(approve), ..Self::default() }
    }
}

fn key_of(id: &str) -> &str {
    id.rsplit('.').next().unwrap_or(id)
}

/// One agent instance on a [`SyncInterpreter`].
pub struct AgentSession {
    interp: SyncInterpreter<DynContext>,
    trace: Option<Arc<AgentTrace>>,
    started: bool,
    restored: bool,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("interp", &self.interp)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl AgentSession {
    /// A fresh session (started on the first [`run`](Self::run)).
    #[must_use]
    pub fn new(machine: Machine, logic: MachineLogic<DynContext>, clock: Arc<dyn Clock>) -> Self {
        Self {
            interp: SyncInterpreter::with_clock(machine, logic, clock),
            trace: None,
            started: false,
            restored: false,
        }
    }

    /// Resume a session from a snapshot (timers resume with their remaining time).
    ///
    /// # Errors
    /// [`AgentError::Engine`] when the snapshot does not fit the machine.
    pub fn restore(
        machine: Machine,
        logic: MachineLogic<DynContext>,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let interp = SyncInterpreter::from_snapshot(
            machine,
            logic,
            snapshot,
            clock,
            &RestoreOptions::default(),
        )?;
        Ok(Self { interp, trace: None, started: true, restored: true })
    }

    /// Attach an observer.
    pub fn observe(&mut self, observer: Arc<dyn Observer<DynContext>>) -> &mut Self {
        self.interp.observe(observer);
        self
    }

    /// Attach an [`AgentTrace`] as observer (it also records `agent_start` on resume).
    pub fn trace(&mut self, trace: Arc<AgentTrace>) -> &mut Self {
        self.interp.observe(Arc::clone(&trace) as Arc<dyn Observer<DynContext>>);
        self.trace = Some(trace);
        self
    }

    /// The interpreter (advance clocks, send other events, inspect state).
    pub fn interpreter_mut(&mut self) -> &mut SyncInterpreter<DynContext> {
        &mut self.interp
    }

    /// The interpreter.
    #[must_use]
    pub const fn interpreter(&self) -> &SyncInterpreter<DynContext> {
        &self.interp
    }

    /// Capture the resting state.
    ///
    /// # Errors
    /// Before the first run.
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.interp.snapshot()?)
    }

    fn ensure_started(&mut self) -> Result<()> {
        if !self.started {
            self.interp.start()?;
            self.started = true;
        } else if self.restored {
            if let Some(t) = &self.trace {
                t.record_resume(self.interp.machine(), &self.interp.state_ids());
            }
            self.restored = false;
        }
        Ok(())
    }

    /// Start (if needed), send the run's events, and report where the machine rests.
    ///
    /// # Errors
    /// Engine failures (a strict chart refusing an event, a failing action).
    pub fn run(&mut self, input: &RunInput) -> Result<AgentResult> {
        self.ensure_started()?;
        let in_idle = self.interp.state_ids().iter().any(|s| key_of(s) == "idle");
        if let Some(prompt) = input.prompt.as_ref().filter(|_| in_idle) {
            self.interp.send(Event::with_payload("START", json!({"prompt": prompt})))?;
        }
        if let Some(ev) = &input.event {
            self.interp.send(ev.clone())?;
        }
        if let Some(approve) = input.approve {
            let ctx = self.interp.context().cloned().unwrap_or(Value::Null);
            self.interp.send(decision_event(&ctx, approve))?;
        }
        Ok(self.result())
    }

    /// Where the machine rests now.
    #[must_use]
    pub fn result(&self) -> AgentResult {
        let ids = self.interp.state_ids();
        let ctx = self.interp.context().cloned().unwrap_or(Value::Null);
        AgentResult {
            final_state: ids.join(","),
            status: self.interp.status().as_str().to_owned(),
            output: ctx.get("result").cloned().unwrap_or(Value::Null),
            error: ctx.get("error").cloned().unwrap_or(Value::Null),
            waiting: ids.iter().any(|s| WAITING_STATES.contains(&key_of(s))),
            context: ctx,
        }
    }

    /// `true` when the machine reached a final state.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.interp.status() == Status::Done
    }
}

/// Options of [`run_agent`].
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Chart (default `TOOL_LOOP`).
    pub chart: Option<Value>,
    /// The events to send.
    pub input: RunInput,
    /// Resume from this snapshot instead of starting fresh.
    pub snapshot: Option<Snapshot>,
    /// Clock (default real time).
    pub clock: Option<Arc<dyn Clock>>,
    /// Observers to attach.
    pub observers: Vec<Arc<dyn Observer<DynContext>>>,
}

/// One [`run_agent`] call: the result and the resting snapshot to resume from.
#[derive(Debug, Clone)]
pub struct AgentRun {
    /// Where the agent rests.
    pub result: AgentResult,
    /// The resting state, for [`RunOptions::snapshot`] / [`AgentSession::restore`].
    pub snapshot: Snapshot,
}

/// Start (or resume) an agent and run it until it rests. The logic's tracer, if any, is
/// attached as an observer too.
///
/// # Errors
/// [`AgentError::Config`] for an invalid agent or chart; engine failures.
pub fn run_agent(logic: &AgentLogic, options: RunOptions) -> Result<AgentRun> {
    let chart = match options.chart {
        Some(c) => c,
        None => crate::charts::tool_loop()?,
    };
    let machine = Machine::from_value(&chart).map_err(|e| AgentError::config(e.to_string()))?;
    let built = logic.build(&machine)?;
    let clock = options.clock.unwrap_or_else(|| Arc::new(RealClock::new()));
    let mut session = match &options.snapshot {
        Some(snap) => AgentSession::restore(machine, built, snap, clock)?,
        None => AgentSession::new(machine, built, clock),
    };
    if let Some(t) = logic.trace() {
        session.trace(Arc::clone(t));
    }
    for obs in options.observers {
        session.observe(obs);
    }
    let result = session.run(&options.input)?;
    Ok(AgentRun { result, snapshot: session.snapshot()? })
}
