//! Multi-agent helpers: [`spawn_agent`], [`BudgetPlugin`] and [`handoff_guard`].
//!
//! There is no supervisor / pipeline / debate runtime: a multi-agent system is an ordinary
//! chart (see [`crate::charts`]) whose actions spawn `TOOL_LOOP` sub-agents. Three helpers make
//! that safe:
//!
//! * [`spawn_agent`] -- an action spawning one sub-agent per sub-task with its OWN budget.
//!   Its tools must be a SUBSET of the parent's allow-list (X0.13): a supervisor cannot mint
//!   a worker that can do more than it may.
//! * [`BudgetPlugin`] -- rolls every child's usage into the PARENT context (`total_usage`,
//!   `usage_by_agent`) before the report is processed, so guards see it, and raises
//!   `BUDGET_EXCEEDED` once; [`spawn_agent`] refuses to spawn after that.
//! * [`handoff_guard`] -- "who may hand off to whom" as a guard over an explicit table, so an
//!   unauthorised handoff is refused by the chart, not by prompt text.
//!
//! # Sync-engine scope
//! [`SyncInterpreter`](xstate_core::SyncInterpreter) runs neither spawned actors nor child
//! machine services (those need the tokio runtime). A spawned sub-agent therefore runs
//! INLINE: the `spawn<Name>` action drives the child to rest on its own interpreter and then
//! raises `AGENT_DONE` / `AGENT_FAILED` (with usage) to the parent -- what the Python sync
//! engine does for a sync model, whose child also finishes inside the action. A child that
//! parks in `awaiting_human` cannot be kept alive inline and is reported as
//! `AGENT_FAILED` with `error.kind == "waiting"`. Long-lived async children, and the
//! runtime re-check of the spawning state's `meta.tools`, are not provided here.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use xstate_core::machine::ActionDef;
use xstate_core::{
    ActionScope, Clock, DynContext, Event, Machine, MachineLogic, Observer, RealClock,
};

use crate::allowlist::validate_agent_chart;
use crate::budgets::{Budget, usage_totals};
use crate::error::engine_err;
use crate::error::{AgentError, Result};
use crate::logic::AgentLogic;
use crate::pyfmt::truthy;
use crate::runner::{AgentSession, RunInput};
use crate::tools::ALL_TOOLS;
use crate::trace::TraceIdentity;

/// Event a child reports success with.
pub const AGENT_DONE: &str = "AGENT_DONE";
/// Event a child reports failure with.
pub const AGENT_FAILED: &str = "AGENT_FAILED";
/// Event raised once when the global budget trips.
pub const BUDGET_EXCEEDED: &str = "BUDGET_EXCEEDED";

const USAGE_KEYS: [&str; 4] = ["turns", "input_tokens", "output_tokens", "cost_usd"];

fn check_subset(child: &[String], parent: &[String], place: &str) -> Result<()> {
    if parent.iter().any(|p| p == ALL_TOOLS) {
        return Ok(());
    }
    let extra: Vec<&String> = child.iter().filter(|c| !parent.contains(c)).collect();
    if extra.is_empty() {
        return Ok(());
    }
    let mut parent_sorted = parent.to_vec();
    parent_sorted.sort();
    Err(AgentError::config(format!(
        "sub-agent tools {extra:?} are not in the parent's allow-list {parent_sorted:?} ({place}); a child may never do more than its parent (X0.13)"
    )))
}

/// Configuration of [`spawn_agent`].
#[derive(Debug, Clone)]
pub struct SpawnAgentConfig {
    /// Action is `spawn` + `Name` (`spawnWorker` for `worker`); child ids are `<name>-<n>`.
    pub name: String,
    /// The child's model, tools and settings (its budget is replaced by `budget`).
    pub logic: AgentLogic,
    /// The child's OWN budget -- required, never inherited.
    pub budget: Budget,
    /// Child chart (default `TOOL_LOOP`).
    pub chart: Option<Value>,
    /// The parent's allow-list. `None` with a child that has tools is refused (closed by
    /// default).
    pub parent_tools: Option<Vec<String>>,
    /// Context key holding the task when the event carries none (default `task`).
    pub task_key: String,
    /// The parent's trace identity (children write into its trace).
    pub parent: TraceIdentity,
    /// Global budget to roll child usage into.
    pub budget_plugin: Option<Arc<BudgetPlugin>>,
    /// Clock for the child interpreters (default real time).
    pub clock: Option<Arc<dyn Clock>>,
}

impl SpawnAgentConfig {
    /// A config with the defaults (`task_key = "task"`, parent identity `supervisor`).
    pub fn new(name: impl Into<String>, logic: AgentLogic, budget: Budget) -> Self {
        Self {
            name: name.into(),
            logic,
            budget,
            chart: None,
            parent_tools: None,
            task_key: "task".into(),
            parent: TraceIdentity::root("supervisor"),
            budget_plugin: None,
            clock: None,
        }
    }
}

/// A validated sub-agent spawner; [`SpawnAgent::apply`] registers its action.
#[derive(Debug)]
pub struct SpawnAgent {
    config: SpawnAgentConfig,
    machine: Machine,
    action: String,
    counter: AtomicU64,
}

/// Validate `config` and build the spawner.
///
/// # Errors
/// [`AgentError::Config`]: child tools exceed `parent_tools`, the child chart lists a tool the
/// registry lacks, or never runs `notifyParent`.
pub fn spawn_agent(config: SpawnAgentConfig) -> Result<SpawnAgent> {
    let child_names = config.logic.registry().names();
    match &config.parent_tools {
        Some(parent) => check_subset(&child_names, parent, "spawn_agent()")?,
        None => check_subset(&child_names, &[], "no parent allow-list")?,
    }
    let chart = match &config.chart {
        Some(c) => c.clone(),
        None => crate::charts::tool_loop()?,
    };
    let machine = Machine::from_value(&chart).map_err(|e| AgentError::config(e.to_string()))?;
    validate_agent_chart(&machine, config.logic.registry())?;
    require_reporting(&machine)?;
    let mut letters = config.name.chars();
    let action = letters.next().map_or_else(
        || "spawn".to_owned(),
        |c| format!("spawn{}{}", c.to_uppercase(), letters.as_str()),
    );
    Ok(SpawnAgent { config, machine, action, counter: AtomicU64::new(1) })
}

/// A child chart with no `notifyParent` never reports; refused at construction.
fn require_reporting(machine: &Machine) -> Result<()> {
    let reports = |actions: &[ActionDef]| {
        actions
            .iter()
            .any(|a| matches!(a, ActionDef::User { name, .. } if &**name == "notifyParent"))
    };
    let found = machine.states().iter().any(|n| reports(&n.entry) || reports(&n.exit))
        || machine.transitions().iter().any(|t| reports(&t.actions));
    if found {
        return Ok(());
    }
    Err(AgentError::config(
        "spawn_agent: the child chart never runs 'notifyParent', so the parent would never receive AGENT_DONE / AGENT_FAILED",
    ))
}

impl SpawnAgent {
    /// The action name (`spawnWorker`).
    #[must_use]
    pub fn action_name(&self) -> &str {
        &self.action
    }

    /// Register the `spawn<Name>` action on `logic`.
    #[must_use]
    pub fn apply(self: &Arc<Self>, logic: MachineLogic<DynContext>) -> MachineLogic<DynContext> {
        let me = Arc::clone(self);
        logic.action(self.action.clone(), move |s: &mut ActionScope<'_, DynContext>| {
            engine_err(me.spawn(s))
        })
    }

    fn task(&self, s: &ActionScope<'_, DynContext>) -> Result<String> {
        let task = s
            .event
            .payload
            .get("task")
            .filter(|t| !t.is_null())
            .or_else(|| s.context.get(&self.config.task_key));
        let Some(task) = task.filter(|t| truthy(t)) else {
            return Err(AgentError::config(format!(
                "{}: no task (event.task or context['{}'])",
                self.action, self.config.task_key
            )));
        };
        task.as_str().map(str::to_owned).ok_or_else(|| {
            AgentError::config(format!("{}: task must be a str, got {task}", self.action))
        })
    }

    fn run_child(&self, id: &str, task: String) -> Result<Value> {
        let logic = self
            .config
            .logic
            .clone()
            .budget(self.config.budget)
            .identity(TraceIdentity::child(id, &self.config.parent));
        let built = logic.build(&self.machine)?;
        let clock = self.config.clock.clone().unwrap_or_else(|| Arc::new(RealClock::new()));
        let mut session = AgentSession::new(self.machine.clone(), built, clock);
        let res = session.run(&RunInput::prompt(task))?;
        let error = if !res.error.is_null() {
            res.error.clone()
        } else if res.waiting {
            json!({"kind": "waiting", "message": "sub-agent is waiting for a human; an inline sub-agent cannot park"})
        } else if !session.is_done() {
            json!({"kind": "incomplete", "message": format!("sub-agent rests in {}", res.final_state)})
        } else {
            Value::Null
        };
        Ok(
            json!({"agent_id": id, "usage": usage_totals(&res.context), "result": res.output, "error": error}),
        )
    }

    fn spawn(&self, s: &mut ActionScope<'_, DynContext>) -> Result<()> {
        if s.context.get("budget_exceeded").is_some_and(truthy) {
            tracing::warn!(target: "xstate::agents", action = %self.action, "global budget exceeded; not spawning");
            return Ok(());
        }
        let task = self.task(s)?;
        let id = s
            .event
            .payload
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty())
            .map_or_else(
                || format!("{}-{}", self.config.name, self.counter.fetch_add(1, Ordering::Relaxed)),
                str::to_owned,
            );
        let report = self.run_child(&id, task)?;
        let tripped =
            self.config.budget_plugin.as_ref().is_some_and(|p| p.rollup(s.context, &report));
        let ty = if report["error"].is_null() { AGENT_DONE } else { AGENT_FAILED };
        s.raise(Event::with_payload(ty, report));
        if tripped {
            let totals = s.context.get("total_usage").cloned().unwrap_or_else(|| json!({}));
            s.raise(Event::with_payload(BUDGET_EXCEEDED, json!({"total_usage": totals})));
        }
        Ok(())
    }
}

/// A usage number from a child's report: non-numeric, negative or non-finite → 0.
fn usage_num(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f.is_finite() && f >= 0.0) => {
            Value::Number(n.clone())
        }
        _ => json!(0),
    }
}

fn add_nums(a: &Value, b: &Value) -> Value {
    match (a.as_u64(), b.as_u64()) {
        (Some(x), Some(y)) => json!(x.saturating_add(y)),
        _ => json!(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
    }
}

/// Rolls child usage into the parent context and enforces a global budget.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetPlugin {
    usd_limit: Option<f64>,
    token_limit: Option<u64>,
    tracked_agents: usize,
}

impl BudgetPlugin {
    /// A global budget; at least one limit is required.
    ///
    /// # Errors
    /// [`AgentError::Config`]: no limit, or a negative / non-finite `max_total_usd`.
    pub fn new(max_total_usd: Option<f64>, max_total_tokens: Option<u64>) -> Result<Self> {
        if max_total_usd.is_none() && max_total_tokens.is_none() {
            return Err(AgentError::config("BudgetPlugin needs at least one limit"));
        }
        if let Some(usd) = max_total_usd
            && (!usd.is_finite() || usd < 0.0)
        {
            return Err(AgentError::config(format!(
                "BudgetPlugin max_total_usd must be a non-negative finite number (got {usd})"
            )));
        }
        Ok(Self { usd_limit: max_total_usd, token_limit: max_total_tokens, tracked_agents: 10_000 })
    }

    /// Bound on `usage_by_agent` entries (oldest evicted).
    #[must_use]
    pub const fn max_tracked_agents(mut self, n: usize) -> Self {
        self.tracked_agents = n;
        self
    }

    /// Whether `totals` reach a global limit.
    #[must_use]
    pub fn exceeded(&self, totals: &Value) -> bool {
        let num = |k: &str| usage_num(totals.get(k)).as_f64().unwrap_or(0.0);
        #[allow(clippy::cast_precision_loss)] // token limits far below 2^53
        let over_tokens = self
            .token_limit
            .is_some_and(|m| num("input_tokens") + num("output_tokens") >= m as f64);
        over_tokens || self.usd_limit.is_some_and(|m| num("cost_usd") >= m)
    }

    /// Register `underGlobalBudget` (false once `budget_exceeded` is set).
    #[must_use]
    pub fn guards(&self, logic: MachineLogic<DynContext>) -> MachineLogic<DynContext> {
        logic.guard("underGlobalBudget", |ctx, _, _| {
            Ok(!ctx.get("budget_exceeded").is_some_and(truthy))
        })
    }

    /// Add one report's usage to `total_usage` / `usage_by_agent`; returns `true` for the one
    /// report that crosses the global limit (it sets `budget_exceeded`).
    pub fn rollup(&self, ctx: &mut Value, report: &Value) -> bool {
        let usage =
            report.get("usage").filter(|u| u.is_object()).cloned().unwrap_or_else(|| json!({}));
        let mut totals =
            ctx.get("total_usage").and_then(Value::as_object).cloned().unwrap_or_default();
        for k in USAGE_KEYS {
            let sum = add_nums(totals.get(k).unwrap_or(&json!(0)), &usage_num(usage.get(k)));
            totals.insert(k.into(), sum);
        }
        let mut by_agent =
            ctx.get("usage_by_agent").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(aid) = report.get("agent_id").filter(|a| truthy(a)).map(crate::pyfmt::py_str) {
            by_agent.shift_remove(&aid);
            let entry: Map<String, Value> =
                USAGE_KEYS.iter().map(|k| ((*k).to_owned(), usage_num(usage.get(*k)))).collect();
            by_agent.insert(aid, Value::Object(entry));
            while by_agent.len() > self.tracked_agents {
                let Some(oldest) = by_agent.keys().next().cloned() else { break };
                by_agent.shift_remove(&oldest);
            }
        }
        let totals = Value::Object(totals);
        let trip = !ctx.get("budget_exceeded").is_some_and(truthy) && self.exceeded(&totals);
        crate::logic::set(ctx, "total_usage", totals);
        crate::logic::set(ctx, "usage_by_agent", Value::Object(by_agent));
        if trip {
            crate::logic::set(ctx, "budget_exceeded", Value::Bool(true));
        }
        trip
    }
}

impl Observer<DynContext> for BudgetPlugin {
    fn on_start(
        &self,
        machine: &Machine,
        _state: &xstate_core::ActiveState<DynContext>,
        _report: &xstate_core::StepReport,
    ) {
        if let Err(err) = check_budget_event_declared(machine) {
            tracing::error!(target: "xstate::agents", %err, "budget plugin misconfigured");
        }
    }
}

/// Fail when a `strict` chart does not declare `BUDGET_EXCEEDED` (it could never be
/// delivered).
///
/// # Errors
/// [`AgentError::Config`].
pub fn check_budget_event_declared(machine: &Machine) -> Result<()> {
    if machine.policies().strict && !machine.declared_events().iter().any(|e| e == BUDGET_EXCEEDED)
    {
        return Err(AgentError::config(
            "BudgetPlugin: a strict chart must declare the BUDGET_EXCEEDED event it will receive",
        ));
    }
    Ok(())
}

/// Register guard `name`: `event.to` is in `allowed[event.from]`. Anything not listed is
/// refused -- closed by default; non-string `from` / `to` never match.
#[must_use]
pub fn handoff_guard<I, K, V, S>(
    logic: MachineLogic<DynContext>,
    name: &str,
    allowed: I,
) -> MachineLogic<DynContext>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: IntoIterator<Item = S>,
    S: Into<String>,
{
    let table: std::collections::HashMap<String, Vec<String>> = allowed
        .into_iter()
        .map(|(k, v)| (k.into(), v.into_iter().map(Into::into).collect()))
        .collect();
    logic.guard(name, move |_, e, _| {
        let from = e.payload.get("from").and_then(Value::as_str);
        let to = e.payload.get("to").and_then(Value::as_str);
        Ok(matches!((from, to), (Some(f), Some(t)) if table.get(f).is_some_and(|dst| dst.iter().any(|d| d == t))))
    })
}
