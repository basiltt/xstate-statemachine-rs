//! [`AgentTrace`]: JSONL agent traces with `gen_ai.*` field names and a cost rollup.
//!
//! One trace per actor TREE: every record carries `trace_id` (the root agent id),
//! `agent_id` and `parent_id`, so a supervisor and the sub-agents it spawned land in one
//! file and [`AgentTrace::totals`] rolls cost up per agent and overall.
//!
//! Memory: [`TraceSink::Memory`] keeps the records for [`AgentTrace::records`], the newest
//! [`max_records`](AgentTrace::max_records) of them (default [`DEFAULT_MAX_RECORDS`]); every
//! other sink streams and retains nothing. [`AgentTrace::totals`] is kept as a running rollup,
//! so it stays exact whatever the sink and however many records were dropped.
//!
//! X0.13 hygiene: `record_content(false)` (the default) writes NO prompt, completion, tool
//! arguments or tool output -- only names, states, token counts and cost. With
//! `record_content(true)` the content is passed through key-based redaction first.
//!
//! Attach the trace as an [`Observer`] for lifecycle / transition records AND pass it to
//! [`AgentLogic::tracer`](crate::AgentLogic::tracer) for model / tool records.
//!
//! # Differences from Python
//! Rust observers see one processed event at a time, so the `state` of an intermediate
//! `transition` record is the transition's target leaves (exact for `TOOL_LOOP`, whose targets
//! are all leaves) and the last one is the configuration after the step. `agent_error`'s
//! `reason` is the engine error kind, not an exception class.

use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use xstate_core::engine::{ActiveState, StepReport};
use xstate_core::{DynContext, Error, Event, Machine, Observer, Status};

use crate::pyfmt::py_dumps;
use crate::scrub::agent_redact_keys;

/// Who a record is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceIdentity {
    /// This agent.
    pub agent_id: String,
    /// The agent that spawned it, if any.
    pub parent_id: Option<String>,
    /// The root of the actor tree.
    pub trace_id: String,
}

impl TraceIdentity {
    /// A root agent: its own trace.
    pub fn root(agent_id: impl Into<String>) -> Self {
        let id = agent_id.into();
        Self { trace_id: id.clone(), agent_id: id, parent_id: None }
    }

    /// A sub-agent of `parent`, in the parent's trace.
    pub fn child(agent_id: impl Into<String>, parent: &Self) -> Self {
        Self {
            agent_id: agent_id.into(),
            parent_id: Some(parent.agent_id.clone()),
            trace_id: parent.trace_id.clone(),
        }
    }
}

/// Default bound on the records [`TraceSink::Memory`] retains.
pub const DEFAULT_MAX_RECORDS: usize = 10_000;

/// Callback receiving each record.
pub type RecordFn = Arc<dyn Fn(&Value) + Send + Sync>;
/// `() -> epoch seconds` for the `ts` field.
pub type TraceClock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Where records go besides [`AgentTrace::records`].
pub enum TraceSink {
    /// Memory only.
    Memory,
    /// A JSONL file, appended to (created with mode `0o600` on Unix: records can hold prompts
    /// and tool output).
    File(PathBuf),
    /// Any writer (one JSON line per record).
    Writer(Mutex<Box<dyn Write + Send>>),
    /// A callback, invoked with no internal lock held (it may call back into the trace).
    Callback(RecordFn),
}

impl fmt::Debug for TraceSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory => f.write_str("Memory"),
            Self::File(p) => write!(f, "File({})", p.display()),
            Self::Writer(_) => f.write_str("Writer(..)"),
            Self::Callback(_) => f.write_str("Callback(..)"),
        }
    }
}

/// Writes one JSON object per agent step.
pub struct AgentTrace {
    sink: TraceSink,
    record_content: bool,
    on_span: Option<RecordFn>,
    clock: TraceClock,
    identity: Option<TraceIdentity>,
    max_records: usize,
    /// The newest records; only filled for [`TraceSink::Memory`].
    records: Mutex<VecDeque<Value>>,
    /// Per-agent usage rolled up from `model_call` records (see [`AgentTrace::totals`]).
    rollup: Mutex<Map<String, Value>>,
    /// Serialises file appends so lines of concurrent records never interleave.
    file_lock: Mutex<()>,
}

impl fmt::Debug for AgentTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTrace")
            .field("sink", &self.sink)
            .field("record_content", &self.record_content)
            .finish_non_exhaustive()
    }
}

impl Default for AgentTrace {
    fn default() -> Self {
        Self::new(TraceSink::Memory)
    }
}

fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn operation(kind: &str) -> &str {
    match kind {
        "model_call" => "chat",
        "tool_call" | "tool_denied" => "execute_tool",
        other => other,
    }
}

impl AgentTrace {
    /// A trace writing to `sink`.
    #[must_use]
    pub fn new(sink: TraceSink) -> Self {
        Self {
            sink,
            record_content: false,
            on_span: None,
            clock: Arc::new(epoch_seconds),
            identity: None,
            max_records: DEFAULT_MAX_RECORDS,
            records: Mutex::new(VecDeque::new()),
            rollup: Mutex::new(Map::new()),
            file_lock: Mutex::new(()),
        }
    }

    /// Bound on the records a [`TraceSink::Memory`] trace retains (oldest dropped first;
    /// default [`DEFAULT_MAX_RECORDS`]). Other sinks retain nothing.
    #[must_use]
    pub const fn max_records(mut self, n: usize) -> Self {
        self.max_records = n;
        self
    }

    /// Include prompts / completions / tool I/O (redacted).
    #[must_use]
    pub const fn record_content(mut self, flag: bool) -> Self {
        self.record_content = flag;
        self
    }

    /// The documented seam for a span exporter: called with every record.
    #[must_use]
    pub fn on_span(mut self, f: RecordFn) -> Self {
        self.on_span = Some(f);
        self
    }

    /// Clock for the `ts` field.
    #[must_use]
    pub fn clock(mut self, clock: TraceClock) -> Self {
        self.clock = clock;
        self
    }

    /// Identity used for observer records (default: the machine id as a root agent).
    #[must_use]
    pub fn identity(mut self, identity: TraceIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The retained records, oldest first: the newest `max_records` of a
    /// [`TraceSink::Memory`] trace, always empty for any other sink.
    pub fn records(&self) -> Vec<Value> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect()
    }

    /// Append one record. Called by the agent logic and the observer hooks; `fields` are the
    /// Python keyword arguments (`response`, `messages`, `tool`, `reason`, `usage`,
    /// `arguments`, `output`).
    pub fn record(
        &self,
        who: &TraceIdentity,
        kind: &str,
        state: &[String],
        fields: &Map<String, Value>,
    ) -> Value {
        let mut rec = Map::new();
        rec.insert("ts".into(), json!((self.clock)()));
        rec.insert("trace_id".into(), json!(who.trace_id));
        rec.insert("agent_id".into(), json!(who.agent_id));
        rec.insert("parent_id".into(), json!(who.parent_id));
        rec.insert("kind".into(), json!(kind));
        rec.insert("gen_ai.operation.name".into(), json!(operation(kind)));
        let mut sorted = state.to_vec();
        sorted.sort();
        rec.insert("state".into(), json!(sorted));
        self.add_fields(&mut rec, fields);
        let rec = Value::Object(rec);
        if rec["kind"] == "model_call" {
            let mut rollup = self.rollup.lock().unwrap_or_else(PoisonError::into_inner);
            add_to_rollup(&mut rollup, &rec);
        }
        if matches!(self.sink, TraceSink::Memory) {
            let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.push_back(rec.clone());
            while records.len() > self.max_records {
                records.pop_front();
            }
        }
        // No lock is held from here on: sinks and the span hook may call back into the trace.
        self.write(&rec);
        if let Some(f) = &self.on_span {
            f(&rec);
        }
        rec
    }

    fn add_fields(&self, out: &mut Map<String, Value>, f: &Map<String, Value>) {
        let keys = agent_redact_keys();
        if let Some(resp) = f.get("response").filter(|r| r.is_object()) {
            self.response_fields(out, resp, f.get("messages"));
        }
        for (key, name) in [("tool", "gen_ai.tool.name"), ("reason", "reason"), ("usage", "usage")]
        {
            if let Some(v) = f.get(key) {
                out.insert(name.into(), v.clone());
            }
        }
        if self.record_content {
            for key in ["arguments", "output"] {
                if let Some(v) = f.get(key) {
                    out.insert(format!("gen_ai.tool.{key}"), keys.redact(v));
                }
            }
        }
    }

    fn response_fields(
        &self,
        out: &mut Map<String, Value>,
        resp: &Value,
        messages: Option<&Value>,
    ) {
        let usage = resp.get("usage").cloned().unwrap_or(Value::Null);
        let model =
            resp.get("model").filter(|m| crate::pyfmt::truthy(m)).cloned().unwrap_or(Value::Null);
        out.insert("gen_ai.request.model".into(), model);
        for (name, key) in [
            ("gen_ai.usage.input_tokens", "input_tokens"),
            ("gen_ai.usage.output_tokens", "output_tokens"),
        ] {
            out.insert(name.into(), json!(crate::budgets::ctx_int(&usage, key)));
        }
        out.insert("cost_usd".into(), json!(crate::budgets::ctx_float(&usage, "cost_usd")));
        let names: Vec<Value> = resp
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| c.get("name").cloned().unwrap_or(Value::Null))
            .collect();
        out.insert("gen_ai.response.tool_calls".into(), Value::Array(names));
        if self.record_content {
            let keys = agent_redact_keys();
            let text = keys.redact(resp.get("text").unwrap_or(&json!("")));
            out.insert("gen_ai.completion".into(), text);
            out.insert("gen_ai.prompt".into(), keys.redact(messages.unwrap_or(&json!([]))));
        }
    }

    fn write(&self, rec: &Value) {
        let line = || format!("{}\n", py_dumps(rec));
        let outcome = match &self.sink {
            TraceSink::Memory => Ok(()),
            TraceSink::Callback(f) => {
                f(rec);
                Ok(())
            }
            TraceSink::File(path) => {
                let _serial = self.file_lock.lock().unwrap_or_else(PoisonError::into_inner);
                open_private(path).and_then(|mut fh| fh.write_all(line().as_bytes()))
            }
            TraceSink::Writer(w) => {
                w.lock().unwrap_or_else(PoisonError::into_inner).write_all(line().as_bytes())
            }
        };
        if let Err(err) = outcome {
            tracing::warn!(target: "xstate::agents", %err, "trace sink write failed");
        }
    }

    /// `{"agents": {agent_id: usage}, "total": usage}` from every `model_call` record seen
    /// (a running rollup: independent of the sink and of [`max_records`](Self::max_records)).
    pub fn totals(&self) -> Value {
        let agents = self.rollup.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let sum = |k: &str| agents.values().map(|a| crate::budgets::ctx_int(a, k)).sum::<i64>();
        let cost: Value = if agents.is_empty() {
            json!(0)
        } else {
            json!(agents.values().map(|a| crate::budgets::ctx_float(a, "cost_usd")).sum::<f64>())
        };
        let total = json!({"turns": sum("turns"), "input_tokens": sum("input_tokens"), "output_tokens": sum("output_tokens"), "cost_usd": cost});
        json!({"agents": agents, "total": total})
    }

    fn who(&self, machine: &Machine) -> TraceIdentity {
        self.identity.clone().unwrap_or_else(|| TraceIdentity::root(machine.id()))
    }

    /// The `agent_start` record of an agent resumed from a snapshot (no `on_start` fires
    /// on restore).
    pub fn record_resume(&self, machine: &Machine, state: &[String]) {
        self.record(&self.who(machine), "agent_start", state, &Map::new());
    }

    fn record_transitions(
        &self,
        machine: &Machine,
        report: &StepReport,
        leaves: &[String],
        status: Status,
    ) {
        let who = self.who(machine);
        let n = report.transitions.len();
        for (i, &tid) in report.transitions.iter().enumerate() {
            let last = i + 1 == n;
            if last && status == Status::Done {
                self.record(&who, "agent_end", leaves, &Map::new());
            }
            let targets = &machine.transition(tid).targets;
            let state: Vec<String> = if last || targets.is_empty() {
                leaves.to_vec()
            } else {
                targets
                    .iter()
                    .flat_map(|&t| machine.leaves(t))
                    .map(|s| machine.node(s).id.to_string())
                    .collect()
            };
            self.record(&who, "transition", &state, &Map::new());
        }
    }
}

/// Fold one `model_call` record into the per-agent usage map.
fn add_to_rollup(agents: &mut Map<String, Value>, r: &Value) {
    let id = crate::pyfmt::py_str(&r["agent_id"]);
    let entry = agents.entry(id).or_insert_with(
        || json!({"turns": 0, "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0}),
    );
    entry["turns"] = json!(crate::budgets::ctx_int(entry, "turns") + 1);
    for (k, src) in [
        ("input_tokens", "gen_ai.usage.input_tokens"),
        ("output_tokens", "gen_ai.usage.output_tokens"),
    ] {
        entry[k] = json!(crate::budgets::ctx_int(entry, k) + crate::budgets::ctx_int(r, src));
    }
    entry["cost_usd"] = json!(
        crate::budgets::ctx_float(entry, "cost_usd") + crate::budgets::ctx_float(r, "cost_usd")
    );
}

/// Open `path` for appending, creating it readable and writable by its owner only on Unix.
fn open_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Observer<DynContext> for AgentTrace {
    fn on_start(&self, machine: &Machine, state: &ActiveState<DynContext>, report: &StepReport) {
        let who = self.who(machine);
        self.record(&who, "agent_start", &[], &Map::new());
        let leaves = state.leaf_ids(machine);
        let initial: Vec<String> = report.transitions.first().map_or_else(
            || leaves.clone(),
            |&t| {
                machine
                    .leaves(machine.transition(t).source)
                    .into_iter()
                    .map(|s| machine.node(s).id.to_string())
                    .collect()
            },
        );
        self.record(&who, "transition", &initial, &Map::new());
        self.record_transitions(machine, report, &leaves, state.status());
    }

    fn on_event(
        &self,
        machine: &Machine,
        _event: &Event,
        report: &StepReport,
        state: &ActiveState<DynContext>,
    ) {
        self.record_transitions(machine, report, &state.leaf_ids(machine), state.status());
    }

    fn on_error(&self, machine: &Machine, error: &Error) {
        let mut fields = Map::new();
        fields.insert("reason".into(), json!(error.kind().to_string()));
        self.record(&self.who(machine), "agent_error", &[], &fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_carry_gen_ai_fields() {
        let seen = Arc::new(Mutex::new(0_usize));
        let counter = Arc::clone(&seen);
        let trace = AgentTrace::new(TraceSink::Callback(Arc::new(|_| {})))
            .record_content(true)
            .clock(Arc::new(|| 1.5))
            .on_span(Arc::new(move |_| {
                *counter.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            }));
        let who = TraceIdentity::child("w-1", &TraceIdentity::root("sup"));
        let mut f = Map::new();
        f.insert("response".into(), json!({"text": "t", "model": "", "usage": {"input_tokens": 3, "output_tokens": 2, "cost_usd": 0.5}, "tool_calls": [{"name": "x"}]}));
        f.insert("messages".into(), json!([{"role": "user", "content": "hi", "token": "s"}]));
        let rec = trace.record(&who, "model_call", &["b".into(), "a".into()], &f);
        assert_eq!(rec["state"], json!(["a", "b"]));
        assert_eq!(rec["gen_ai.operation.name"], "chat");
        assert_eq!(rec["gen_ai.request.model"], Value::Null);
        assert_eq!(rec["gen_ai.prompt"][0]["token"], "***");
        assert_eq!(rec["parent_id"], "sup");
        assert_eq!(*seen.lock().unwrap_or_else(PoisonError::into_inner), 1);
        let totals = trace.totals();
        assert_eq!(totals["agents"]["w-1"]["input_tokens"], 3);
        assert_eq!(totals["total"]["cost_usd"], 0.5);
        assert_eq!(AgentTrace::default().totals()["total"]["cost_usd"], 0);
    }

    #[test]
    fn sinks_write_jsonl() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("t.jsonl");
        let trace = AgentTrace::new(TraceSink::File(path.clone())).clock(Arc::new(|| 0.0));
        let mut f = Map::new();
        f.insert("tool".into(), json!("x"));
        f.insert("arguments".into(), json!({"a": 1}));
        trace.record(&TraceIdentity::root("a"), "tool_call", &[], &f);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            text.starts_with("{\"ts\": 0.0, \"trace_id\": \"a\"")
                && text.contains("\"gen_ai.tool.name\": \"x\"")
        );
        assert!(!text.contains("gen_ai.tool.arguments"));
        assert!(trace.records().is_empty(), "a file trace retains nothing in memory");
        let w = AgentTrace::new(TraceSink::Writer(Mutex::new(Box::new(Vec::new()))));
        w.record(&TraceIdentity::root("a"), "x", &[], &Map::new());
        assert_eq!(w.records().len(), 0);
        assert!(format!("{w:?}").contains("Writer"));
        let bad = AgentTrace::new(TraceSink::File(dir.path().join("missing").join("t.jsonl")));
        bad.record(&TraceIdentity::root("a"), "x", &[], &Map::new());
        assert_eq!(bad.records().len(), 0);
    }

    #[test]
    fn memory_sink_is_bounded_and_totals_survive_the_bound() {
        let trace = AgentTrace::default().max_records(3).clock(Arc::new(|| 0.0));
        let who = TraceIdentity::root("a");
        let mut f = Map::new();
        f.insert("response".into(), json!({"text": "t", "usage": {"input_tokens": 2}}));
        for _ in 0..5 {
            trace.record(&who, "model_call", &[], &f);
        }
        trace.record(&who, "tool_call", &[], &Map::new());
        let kinds: Vec<Value> = trace.records().iter().map(|r| r["kind"].clone()).collect();
        assert_eq!(kinds, ["model_call", "model_call", "tool_call"]);
        assert_eq!(trace.totals()["total"]["turns"], 5);
        assert_eq!(trace.totals()["total"]["input_tokens"], 10);
    }

    #[test]
    fn callbacks_run_without_the_trace_locks_held() {
        let trace = Arc::new_cyclic(|weak: &std::sync::Weak<AgentTrace>| {
            let weak = weak.clone();
            let reenter: RecordFn = Arc::new(move |_| {
                if let Some(trace) = weak.upgrade() {
                    let _ = (trace.records(), trace.totals());
                }
            });
            AgentTrace::new(TraceSink::Callback(Arc::clone(&reenter))).on_span(reenter)
        });
        trace.record(&TraceIdentity::root("a"), "x", &[], &Map::new());
    }

    #[cfg(unix)]
    #[test]
    fn trace_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("t.jsonl");
        let trace = AgentTrace::new(TraceSink::File(path.clone()));
        trace.record(&TraceIdentity::root("a"), "x", &[], &Map::new());
        let mode = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{e}")).permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
