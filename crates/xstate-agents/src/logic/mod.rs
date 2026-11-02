//! [`AgentLogic`]: the `MachineLogic` that drives the `TOOL_LOOP` chart.
//!
//! "The model proposes, the machine decides." The model is ONE invoked service
//! (`callModel`); everything that matters for safety is chart structure plus the guards and
//! the `runTool` service:
//!
//! * budgets are guards on the single entry to a model turn (`checking_budget`), so no path
//!   reaches the model over budget;
//! * which tools the model is TOLD about and which may RUN are the `meta.tools` allow-lists
//!   of `awaiting_model` / `awaiting_tool`;
//! * `runTool` re-checks the allow-list, the argument schema and human approval itself
//!   (X0.13) -- a guard is advisory, the service is not;
//! * `awaiting_human` is an ordinary state, so it persists in any snapshot and its `after`
//!   escalation is a durable deadline.
//!
//! The context is a [`DynContext`] (`serde_json::Value`) with the Python key names, so a
//! snapshot written by either implementation reads the same.

mod actions;
mod call_model;
mod guards;
mod retry;
mod run_tool;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};
use xstate_core::{DynContext, Event, Machine, MachineLogic};

pub use retry::AgentRetry;

use crate::allowlist::{hosting_meta, hosting_states, hosting_tools};
use crate::budgets::Budget;
use crate::error::{AgentError, Result};
use crate::messages::ModelCall;
use crate::output::{OutputParser, resolve_output_model};
use crate::tools::ToolRegistry;
use crate::trace::{AgentTrace, TraceIdentity};

/// `(messages) -> shorter messages`, used when the transcript exceeds `max_messages`.
pub type Summariser = Arc<dyn Fn(Vec<Value>) -> Vec<Value> + Send + Sync>;

/// How many tool-call ids are remembered for the duplicate check.
pub const SPENT_IDS: usize = 1000;

/// Configuration of one agent; [`AgentLogic::build`] turns it into the `MachineLogic` for a
/// chart (`TOOL_LOOP`, or any chart reusing its names).
#[derive(Clone)]
pub struct AgentLogic {
    model: Arc<dyn ModelCall>,
    tools: Arc<ToolRegistry>,
    budget: Budget,
    output_model: Option<Value>,
    output_models: BTreeMap<String, Value>,
    max_messages: usize,
    summarise: Option<Summariser>,
    max_output_retries: u32,
    system_prompt: Option<String>,
    model_timeout_s: f64,
    human_timeout_s: f64,
    retry: AgentRetry,
    tracer: Option<Arc<AgentTrace>>,
    identity: Option<TraceIdentity>,
    max_tool_calls: usize,
    output_parser: Option<OutputParser>,
}

impl fmt::Debug for AgentLogic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentLogic")
            .field("tools", &self.tools.names())
            .field("budget", &self.budget)
            .field("max_messages", &self.max_messages)
            .field("max_tool_calls", &self.max_tool_calls)
            .finish_non_exhaustive()
    }
}

impl AgentLogic {
    /// An agent around `model` with no tools and the Python defaults: 10 turns,
    /// 200 messages, 2 output retries, 60 s model / 24 h human timeouts, 8 tool calls per
    /// turn, retry 3 attempts from 1 s.
    pub fn new(model: Arc<dyn ModelCall>) -> Self {
        Self {
            model,
            tools: Arc::new(ToolRegistry::default()),
            budget: Budget::default(),
            output_model: None,
            output_models: BTreeMap::new(),
            max_messages: 200,
            summarise: None,
            max_output_retries: 2,
            system_prompt: None,
            model_timeout_s: 60.0,
            human_timeout_s: 24.0 * 3600.0,
            retry: AgentRetry::default(),
            tracer: None,
            identity: None,
            max_tool_calls: 8,
            output_parser: None,
        }
    }

    /// The tool registry.
    #[must_use]
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    /// The budget enforced by the `checking_budget` guards.
    #[must_use]
    pub const fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// JSON schema every final reply must match (else a state's `meta.output_model`, else
    /// free text).
    #[must_use]
    pub fn output_model(mut self, schema: Option<Value>) -> Self {
        self.output_model = schema;
        self
    }

    /// Register a named schema a chart can reference as `meta.output_model: "<name>"`.
    #[must_use]
    pub fn named_output_model(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.output_models.insert(name.into(), schema);
        self
    }

    /// Bound on `context["messages"]` (>= 2); the oldest turns after the task are dropped
    /// unless a summariser is set.
    #[must_use]
    pub const fn max_messages(mut self, n: usize) -> Self {
        self.max_messages = n;
        self
    }

    /// Hook that shortens an over-long transcript.
    #[must_use]
    pub fn summarise(mut self, f: Summariser) -> Self {
        self.summarise = Some(f);
        self
    }

    /// Output-validation re-prompts before `error`.
    #[must_use]
    pub const fn max_output_retries(mut self, n: u32) -> Self {
        self.max_output_retries = n;
        self
    }

    /// Prepended to every model request (never stored in context or snapshots).
    #[must_use]
    pub fn system_prompt(mut self, text: impl Into<String>) -> Self {
        self.system_prompt = Some(text.into());
        self
    }

    /// The `modelTimeout` / `humanTimeout` delays in seconds.
    #[must_use]
    pub const fn timeouts(mut self, model_s: f64, human_s: f64) -> Self {
        self.model_timeout_s = model_s;
        self.human_timeout_s = human_s;
        self
    }

    /// Retry policy for `timed_out`.
    #[must_use]
    pub const fn retry(mut self, retry: AgentRetry) -> Self {
        self.retry = retry;
        self
    }

    /// Trace receiving `model_call` / `tool_call` / `tool_denied` records.
    #[must_use]
    pub fn tracer(mut self, trace: Arc<AgentTrace>) -> Self {
        self.tracer = Some(trace);
        self
    }

    /// Ids written on trace records and reports (default: the machine id).
    #[must_use]
    pub fn identity(mut self, identity: TraceIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// More tool calls than this in ONE model turn is denied.
    #[must_use]
    pub const fn max_tool_calls(mut self, n: usize) -> Self {
        self.max_tool_calls = n;
        self
    }

    /// Parser applied to the reply before structured validation (default strict JSON).
    #[must_use]
    pub fn output_parser(mut self, parser: OutputParser) -> Self {
        self.output_parser = Some(parser);
        self
    }

    /// The registry.
    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.tools
    }

    /// The budget.
    #[must_use]
    pub const fn agent_budget(&self) -> &Budget {
        &self.budget
    }

    /// The tracer, if any.
    #[must_use]
    pub fn trace(&self) -> Option<&Arc<AgentTrace>> {
        self.tracer.as_ref()
    }

    /// Build the `MachineLogic` for `machine`: actions, guards, services, delays, budget
    /// guards and the retry trio (`retryBump`, `retryReset`, `retryCanRetry`, `retryDelay`).
    ///
    /// # Errors
    /// [`AgentError::Config`]: `max_messages < 2`, a bad output model, or a malformed
    /// `meta.tools` on a state hosting `callModel` / `runTool`.
    pub fn build(&self, machine: &Machine) -> Result<MachineLogic<DynContext>> {
        let core = Arc::new(self.core(machine)?);
        let logic = actions::register(MachineLogic::dynamic(), &core);
        let logic = guards::register(logic, &core);
        let logic = crate::budgets::budget_guards(logic, &self.budget);
        let logic = self.retry.apply(logic);
        let model_core = Arc::clone(&core);
        let tool_core = Arc::clone(&core);
        let delay_core = Arc::clone(&core);
        Ok(logic
            .service("callModel", move |ctx, _| {
                model_core.call_model(ctx).map_err(AgentError::into_engine)
            })
            .service("runTool", move |ctx, _| {
                tool_core.run_tool(ctx).map_err(AgentError::into_engine)
            })
            .delay("modelTimeout", self.model_timeout_s * 1000.0)
            .delay("humanTimeout", self.human_timeout_s * 1000.0)
            .delay_fn("toolTimeout", move |ctx, _| Ok(delay_core.tool_timeout_ms(ctx))))
    }

    fn core(&self, machine: &Machine) -> Result<Core> {
        if self.max_messages < 2 {
            return Err(AgentError::config("max_messages must be >= 2"));
        }
        crate::allowlist::validate_agent_chart(machine, &self.tools)?;
        let output_model = self
            .output_model
            .as_ref()
            .map(|s| resolve_output_model(s, &self.output_models))
            .transpose()?;
        let model_spec = hosting_meta(machine, "callModel", "output_model");
        if let Some(spec) = &model_spec {
            resolve_output_model(spec, &self.output_models)?;
        }
        let ids = |src: &str| -> Vec<String> {
            let mut v: Vec<String> = hosting_states(machine, src)
                .into_iter()
                .map(|s| machine.node(s).id.to_string())
                .collect();
            v.sort();
            v
        };
        Ok(Core {
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            output_model,
            output_models: self.output_models.clone(),
            max_messages: self.max_messages,
            summarise: self.summarise.clone(),
            max_output_retries: self.max_output_retries,
            system_prompt: self.system_prompt.clone(),
            tracer: self.tracer.clone(),
            identity: self.identity.clone().unwrap_or_else(|| TraceIdentity::root(machine.id())),
            max_tool_calls: self.max_tool_calls,
            output_parser: self.output_parser.clone(),
            model_tools: hosting_tools(machine, "callModel")?,
            tool_tools: hosting_tools(machine, "runTool")?,
            model_states: ids("callModel"),
            tool_states: ids("runTool"),
            model_output_spec: model_spec,
        })
    }
}

/// Per-build state shared by the services, guards and actions.
pub(crate) struct Core {
    model: Arc<dyn ModelCall>,
    tools: Arc<ToolRegistry>,
    output_model: Option<Value>,
    output_models: BTreeMap<String, Value>,
    max_messages: usize,
    summarise: Option<Summariser>,
    max_output_retries: u32,
    system_prompt: Option<String>,
    tracer: Option<Arc<AgentTrace>>,
    identity: TraceIdentity,
    max_tool_calls: usize,
    output_parser: Option<OutputParser>,
    model_tools: Vec<String>,
    tool_tools: Vec<String>,
    model_states: Vec<String>,
    tool_states: Vec<String>,
    model_output_spec: Option<Value>,
}

impl Core {
    fn trace(&self, kind: &str, state: &[String], fields: &Map<String, Value>) {
        if let Some(t) = &self.tracer {
            t.record(&self.identity, kind, state, fields);
        }
    }

    /// Keep the task (first message) and the most recent tail; never strand `tool` results
    /// whose assistant turn was cut.
    fn bound(&self, messages: Vec<Value>) -> Result<Vec<Value>> {
        if messages.len() <= self.max_messages {
            return Ok(messages);
        }
        if let Some(f) = &self.summarise {
            let out = f(messages);
            if out.len() > self.max_messages {
                return Err(AgentError::config(
                    "summarise() must return at most max_messages messages",
                ));
            }
            return Ok(out);
        }
        let start = messages.len() - (self.max_messages - 1);
        let mut tail = &messages[start..];
        while tail.first().is_some_and(|m| m.get("role").and_then(Value::as_str) == Some("tool")) {
            tail = &tail[1..];
        }
        Ok(messages[..1].iter().chain(tail).cloned().collect())
    }

    fn append(&self, ctx: &mut Value, msgs: impl IntoIterator<Item = Value>) -> Result<()> {
        let mut all: Vec<Value> =
            ctx.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
        all.extend(msgs);
        set(ctx, "messages", Value::Array(self.bound(all)?));
        Ok(())
    }

    /// Constructor-level schema, else the `output_model` spec `callModel` recorded.
    fn output_model_for(&self, data: &Value) -> Option<Value> {
        if self.output_model.is_some() {
            return self.output_model.clone();
        }
        data.get("output_model")
            .and_then(|spec| resolve_output_model(spec, &self.output_models).ok())
    }

    fn tool_timeout_ms(&self, ctx: &Value) -> f64 {
        let total: f64 = ctx
            .get("pending_tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .map(|n| self.tools.get(n).map_or(0.0, crate::tools::Tool::timeout_s))
            .sum();
        1000.0 * (total + 1.0)
    }
}

/// `event.output` when it is an object (a service result), else `{}`.
pub(crate) fn data_of(event: &Event) -> &Value {
    static EMPTY: std::sync::LazyLock<Value> =
        std::sync::LazyLock::new(|| Value::Object(Map::new()));
    let out = event.output();
    if out.is_object() { out } else { &EMPTY }
}

/// Insert `key` into an object context (making it an object if needed).
pub(crate) fn set(ctx: &mut Value, key: &str, value: Value) {
    if !ctx.is_object() {
        *ctx = Value::Object(Map::new());
    }
    if let Value::Object(map) = ctx {
        map.insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::FakeModel;
    use serde_json::json;

    fn core(max: usize) -> Core {
        let machine =
            Machine::from_value(&crate::charts::tool_loop().unwrap_or_else(|e| panic!("{e}")))
                .unwrap_or_else(|e| panic!("{e}"));
        AgentLogic::new(Arc::new(FakeModel::new([])))
            .max_messages(max)
            .core(&machine)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn bounding_keeps_task_and_drops_orphans() {
        let c = core(3);
        let msgs = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "a"}),
            json!({"role": "tool", "content": "r"}),
            json!({"role": "assistant", "content": "b"}),
        ];
        let out = c.bound(msgs).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            out,
            vec![
                json!({"role": "user", "content": "task"}),
                json!({"role": "assistant", "content": "b"})
            ]
        );
        assert_eq!(c.model_tools, ["*"]);
        assert_eq!(c.tool_states, ["toolLoop.awaiting_tool"]);
    }

    #[test]
    fn summariser_must_shrink() {
        let mut c = core(2);
        c.summarise = Some(Arc::new(|m: Vec<Value>| m));
        assert!(c.bound(vec![json!(1), json!(2), json!(3)]).is_err());
        c.summarise = Some(Arc::new(|m: Vec<Value>| m[..1].to_vec()));
        assert_eq!(
            c.bound(vec![json!(1), json!(2), json!(3)]).unwrap_or_else(|e| panic!("{e}")),
            vec![json!(1)]
        );
    }

    #[test]
    fn build_rejects_bad_config() {
        let machine =
            Machine::from_value(&crate::charts::tool_loop().unwrap_or_else(|e| panic!("{e}")))
                .unwrap_or_else(|e| panic!("{e}"));
        let base = AgentLogic::new(Arc::new(FakeModel::new([])));
        assert!(base.clone().max_messages(1).build(&machine).is_err());
        assert!(base.clone().output_model(Some(json!("Missing"))).build(&machine).is_err());
        assert!(base.output_model(Some(json!({"type": "object"}))).build(&machine).is_ok());
        let mut ctx = json!(null);
        set(&mut ctx, "a", json!(1));
        assert_eq!(ctx, json!({"a": 1}));
    }
}
