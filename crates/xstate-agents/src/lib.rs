//! LLM agents as statecharts: "the model proposes, the machine decides".
//!
//! A Rust port of the provider-independent core of the Python library's `contrib.agents`. An
//! agent is the `TOOL_LOOP` chart ([`charts`]) plus [`AgentLogic`]: budgets are guards on the
//! single entry to a model turn, per-state `meta.tools` allow-lists decide which tools the
//! model is told about and which may run, `runTool` re-checks allow-list, argument schema and
//! human approval itself (X0.13), and `awaiting_human` is a durable state with an `after`
//! escalation. Everything runs on [`xstate_core::SyncInterpreter`] with a JSON context whose
//! keys match the Python library's, so snapshots read the same in both.
//!
//! ```
//! use std::sync::Arc;
//! use serde_json::json;
//! use xstate_agents::{AgentLogic, FakeModel, RunInput, RunOptions, ToolSpec, run_agent, tool_registry};
//!
//! let model = Arc::new(FakeModel::new([
//!     json!({"tool": "lookup", "args": {"query": "meaning"}}),
//!     json!({"text": "it is 42"}),
//! ]));
//! let lookup = ToolSpec::new("lookup", |args| Ok(json!({"answer": 42, "query": args["query"]})))
//!     .parameters(json!({"type": "object", "properties": {"query": {"type": "string"}},
//!                        "required": ["query"], "additionalProperties": false}));
//! let logic = AgentLogic::new(model).tools(tool_registry([lookup])?);
//! let run = run_agent(&logic, RunOptions { input: RunInput::prompt("what is the meaning?"), ..RunOptions::default() })?;
//! assert_eq!(run.result.final_state, "toolLoop.done");
//! assert_eq!(run.result.output, json!("it is 42"));
//! # Ok::<(), xstate_agents::AgentError>(())
//! ```
//!
//! Provider adapters (`OpenAI`, `Anthropic`), framework bridges (`pydantic-ai`, `LangGraph`) and
//! long-lived async sub-agents are out of scope; see [`multi`] for the sync sub-agent story.

#![forbid(unsafe_code)]

pub mod allowlist;
pub mod budgets;
pub mod charts;
mod error;
pub mod logic;
pub mod messages;
pub mod multi;
pub mod output;
mod pyfmt;
pub mod runner;
pub mod scrub;
pub mod structured;
pub mod tools;
pub mod trace;

pub use allowlist::{state_tools, validate_agent_chart};
pub use budgets::{Budget, budget_guards};
pub use charts::{CHART_NAMES, TOOL_LOOP_JSON, load_chart};
pub use error::{AgentError, Result, split_event_error};
pub use logic::{AgentLogic, AgentRetry, Summariser};
pub use messages::{
    FakeModel, ModelCall, ModelRequest, ModelResponse, ScriptItem, ToolCall, Usage, calls_from,
};
pub use multi::{
    BudgetPlugin, SpawnAgent, SpawnAgentConfig, check_budget_event_declared, handoff_guard,
    spawn_agent,
};
pub use runner::{
    AgentResult, AgentRun, AgentSession, RunInput, RunOptions, WAITING_STATES, pending_approval,
    run_agent,
};
pub use scrub::{AGENT_REDACT_KEYS, scrub};
pub use structured::{StructuredOutput, json_parser, structured_output, validate_structured};
pub use tools::{
    ALL_TOOLS, DEFAULT_MAX_OUTPUT_CHARS, DEFAULT_TOOL_TIMEOUT_S, Tool, ToolRegistry, ToolSpec,
    tool_registry,
};
pub use trace::{AgentTrace, TraceIdentity, TraceSink};
