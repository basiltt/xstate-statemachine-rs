//! Golden comparison with the Python library: `conformance/tools/agents_golden.py` runs
//! `run_agent_sync` with a `FakeModel` script and two tools and dumps, per run, the
//! `AgentResult` fields, the leaf states after each transition, the trace records and the
//! trace totals. This test replays the same scripts (read from the golden files) and
//! compares `serde_json::Value`s after normalising ids in both.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use xstate_agents::{
    AgentLogic, AgentTrace, Budget, FakeModel, RunInput, RunOptions, ToolRegistry, ToolSpec,
    TraceSink, run_agent,
};

fn registry() -> ToolRegistry {
    let lookup = ToolSpec::new("lookup", |args| {
        Ok(json!({"answer": 42, "query": args["query"], "api_key": "k-123", "note": "Bearer abcdefghijklmnop"}))
    })
    .description("Look a fact up in the knowledge base.")
    .parameters(json!({
        "type": "object", "properties": {"query": {"type": "string"}},
        "required": ["query"], "additionalProperties": false
    }));
    let refund = ToolSpec::new("refund", |args| {
        Ok(json!(format!(
            "refunded {} on {}",
            args["amount_cents"],
            args["order_id"].as_str().unwrap_or_default()
        )))
    })
    .description("Refund an order.")
    .parameters(json!({
        "type": "object",
        "properties": {"order_id": {"type": "string"}, "amount_cents": {"type": "integer"}},
        "required": ["order_id", "amount_cents"], "additionalProperties": false
    }))
    .side_effect(true)
    .timeout_s(5.0);
    ToolRegistry::builder().tool(lookup).tool(refund).build().unwrap_or_else(|e| panic!("{e}"))
}

/// `call_<8 hex>_<n>_<k>` → `call_<n>_<k>`.
fn normalise_call_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("call_") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 5..];
        let hex = tail
            .get(..8)
            .filter(|h| h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        if let Some(after) = hex.and_then(|_| tail.get(8..)).and_then(|a| a.strip_prefix('_')) {
            out.push_str("call_");
            rest = after;
        } else {
            out.push_str("call_");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn normalise(value: &Value, ids: &[String]) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), normalise(v, ids))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| normalise(v, ids)).collect()),
        Value::String(s) if ids.contains(s) => json!("<agent>"),
        Value::String(s) => json!(normalise_call_ids(s)),
        other => other.clone(),
    }
}

fn run_phase(
    model: &Arc<FakeModel>,
    budget: Budget,
    input: &Value,
    snapshot: Option<xstate_core::Snapshot>,
) -> (Value, xstate_core::Snapshot) {
    let trace =
        Arc::new(AgentTrace::new(TraceSink::Memory).record_content(true).clock(Arc::new(|| 0.0)));
    let logic = AgentLogic::new(Arc::clone(model) as Arc<dyn xstate_agents::ModelCall>)
        .tools(registry())
        .budget(budget)
        .tracer(Arc::clone(&trace));
    let run_input = RunInput {
        prompt: input.get("prompt").and_then(Value::as_str).map(str::to_owned),
        event: None,
        approve: input.get("approve").and_then(Value::as_bool),
    };
    let run = run_agent(&logic, RunOptions { input: run_input, snapshot, ..RunOptions::default() })
        .unwrap_or_else(|e| panic!("{e}"));
    let records = trace.records();
    let r = &run.result;
    let states: Vec<Value> = records
        .iter()
        .filter(|rec| rec["kind"] == "transition")
        .map(|rec| rec["state"].clone())
        .collect();
    let phase = json!({
        "input": input,
        "result": {
            "final_state": r.final_state, "status": r.status, "context": r.context, "output": r.output,
            "error": r.error, "waiting": r.waiting, "usage": r.usage(),
        },
        "states": states,
        "trace": records,
        "totals": trace.totals(),
    });
    let mut ids: Vec<String> = records
        .iter()
        .flat_map(|rec| [rec["agent_id"].clone(), rec["trace_id"].clone()])
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids.dedup();
    (normalise(&phase, &ids), run.snapshot)
}

fn check(name: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let golden: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    let script = golden["script"].as_array().cloned().unwrap_or_default();
    let model = Arc::new(FakeModel::new(script));
    let budget = Budget::from_value(&golden["budgets"]).unwrap_or_else(|e| panic!("{e}"));
    let phases = golden["phases"].as_array().cloned().unwrap_or_default();
    assert!(!phases.is_empty(), "{name}: no phases");
    let mut snapshot = None;
    for (i, expected) in phases.iter().enumerate() {
        let (actual, snap) = run_phase(&model, budget, &expected["input"], snapshot.take());
        snapshot = Some(snap);
        if actual != *expected {
            let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
            let a = actual.as_object().cloned().unwrap_or_else(Map::new);
            for (k, v) in &a {
                assert!(
                    expected.get(k) == Some(v),
                    "{name} phase {i}: `{k}` differs\n--- rust ---\n{}\n--- python ---\n{}",
                    pretty(v),
                    pretty(&expected[k])
                );
            }
            panic!("{name} phase {i}: differs\n{}\nvs\n{}", pretty(&actual), pretty(expected));
        }
    }
}

#[test]
fn plain_answer_matches_python() {
    check("plain_answer");
}

#[test]
fn tool_then_answer_matches_python() {
    check("tool_then_answer");
}

#[test]
fn approval_then_resume_matches_python() {
    check("approval");
}

#[test]
fn rejection_matches_python() {
    check("rejected");
}

#[test]
fn budget_exhaustion_matches_python() {
    check("budget_exhausted");
}

#[test]
fn denied_arguments_match_python() {
    check("denied_arguments");
}

#[test]
fn call_ids_normalise() {
    assert_eq!(normalise_call_ids("x call_0a1b2c3d_1_0 y call_short"), "x call_1_0 y call_short");
}
