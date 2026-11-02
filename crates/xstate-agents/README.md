# xstate-agents

LLM agents as statecharts: "the model proposes, the machine decides". A port of the
provider-independent core of the Python library's `contrib.agents`. An agent is the `TOOL_LOOP`
chart plus `AgentLogic`: budgets are guards on the single entry to a model turn, per-state
`meta.tools` allow-lists decide which tools the model is told about and which may run, `runTool`
re-checks allow-list, argument schema and human approval itself, and `awaiting_human` is a
durable state with an `after` escalation. Supervisor, pipeline and debate charts, structured
output, an `AgentTrace` observer and snapshot resume are included.

Status: 0.1.0; six Python golden scenarios match exactly. Provider adapters (OpenAI, Anthropic)
and framework bridges are out of scope: implement the small model trait, or use `FakeModel`.

```rust
use std::sync::Arc;
use serde_json::json;
use xstate_agents::{AgentLogic, FakeModel, RunInput, RunOptions, ToolSpec, run_agent, tool_registry};

let model = Arc::new(FakeModel::new([
    json!({"tool": "lookup", "args": {"query": "meaning"}}),
    json!({"text": "it is 42"}),
]));
let lookup = ToolSpec::new("lookup", |args| Ok(json!({"answer": 42, "query": args["query"]})))
    .parameters(json!({"type": "object", "properties": {"query": {"type": "string"}},
                       "required": ["query"], "additionalProperties": false}));
let logic = AgentLogic::new(model).tools(tool_registry([lookup])?);
let run = run_agent(&logic, RunOptions { input: RunInput::prompt("what is the meaning?"), ..RunOptions::default() })?;
assert_eq!(run.result.output, json!("it is 42"));
# Ok::<(), xstate_agents::AgentError>(())
```

## Features

None.

## Python counterpart

[`xstate_statemachine.contrib.agents`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/contrib/agents).
