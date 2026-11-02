//! Behavioural tests of the `TOOL_LOOP` agent beyond the Python goldens: structured output,
//! retries and timeouts on a simulated clock, denials, budgets and the `runTool` re-check.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_agents::{
    AgentError, AgentLogic, AgentRetry, AgentSession, AgentTrace, Budget, FakeModel, ModelCall,
    RunInput, RunOptions, ScriptItem, ToolRegistry, ToolSpec, load_chart, pending_approval,
    run_agent, structured_output,
};
use xstate_core::{Machine, SimulatedClock};

fn registry() -> ToolRegistry {
    let lookup = ToolSpec::new("lookup", |args| Ok(json!({"answer": 42, "q": args["query"]})))
        .parameters(json!({"type": "object", "properties": {"query": {"type": "string"}},
                           "required": ["query"], "additionalProperties": false}));
    let refund = ToolSpec::new("refund", |_| Ok(json!("refunded")))
        .parameters(json!({"type": "object", "properties": {"order_id": {"type": "string"}},
                           "required": ["order_id"], "additionalProperties": false}))
        .side_effect(true);
    let slow = ToolSpec::new("slow", |_| {
        std::thread::sleep(Duration::from_millis(400));
        Ok(Value::Null)
    })
    .timeout_s(0.05);
    let failing = ToolSpec::new("failing", |_| Err(AgentError::tool("database down")));
    ToolRegistry::builder()
        .tool(lookup)
        .tool(refund)
        .tool(slow)
        .tool(failing)
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
}

fn model(script: Vec<Value>) -> Arc<FakeModel> {
    Arc::new(FakeModel::new(script))
}

fn logic(model: &Arc<FakeModel>) -> AgentLogic {
    AgentLogic::new(Arc::clone(model) as Arc<dyn ModelCall>).tools(registry())
}

fn tool_loop() -> Machine {
    Machine::from_value(&load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn session(logic: &AgentLogic, clock: &Arc<SimulatedClock>) -> AgentSession {
    let machine = tool_loop();
    let built = logic.build(&machine).unwrap_or_else(|e| panic!("{e}"));
    AgentSession::new(machine, built, Arc::clone(clock) as Arc<dyn xstate_core::Clock>)
}

fn run(logic: &AgentLogic, prompt: &str) -> xstate_agents::AgentResult {
    run_agent(logic, RunOptions { input: RunInput::prompt(prompt), ..RunOptions::default() })
        .unwrap_or_else(|e| panic!("{e}"))
        .result
}

#[test]
fn structured_output_retries_then_validates() {
    let m = model(vec![json!({"text": "not json"}), json!({"text": "{\"city\": \"Oslo\"}"})]);
    let schema =
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]});
    let settings = structured_output(Some(schema), 2).unwrap_or_else(|e| panic!("{e}"));
    let res = run(&logic(&m).structured(settings), "weather?");
    assert_eq!(res.final_state, "toolLoop.done");
    assert_eq!(res.output, json!({"city": "Oslo"}));
    assert_eq!(res.context["output_retries"], 1);
    let retry = &res.context["messages"][2]["content"];
    assert!(
        retry
            .as_str()
            .unwrap_or_default()
            .starts_with("RETRY_OUTPUT: your answer did not validate (not valid JSON)."),
        "{retry}"
    );
}

#[test]
fn structured_output_exhaustion_fails() {
    let m = model(vec![json!({"text": "{\"city\": 3}"})]);
    let schema = json!({"type": "object", "properties": {"city": {"type": "string"}}});
    let res = run(&logic(&m).output_model(Some(schema)).max_output_retries(0), "weather?");
    assert_eq!(
        res.error,
        json!({"kind": "output", "message": "model output failed validation (city: Input should be a valid string)"})
    );
}

#[test]
fn per_state_output_models_are_named_schemas() {
    let mut chart = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    chart["states"]["awaiting_model"]["meta"]["output_model"] = json!("Answer");
    let m = model(vec![json!({"text": "{\"n\": 1}"})]);
    let schema =
        json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
    let agent = logic(&m).named_output_model("Answer", schema);
    let run = run_agent(
        &agent,
        RunOptions {
            chart: Some(chart.clone()),
            input: RunInput::prompt("n?"),
            ..RunOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run.result.output, json!({"n": 1}));
    let unnamed = logic(&model(vec![]));
    assert!(
        run_agent(&unnamed, RunOptions { chart: Some(chart), ..RunOptions::default() }).is_err()
    );
}

#[test]
fn model_failures_retry_on_the_clock_then_exhaust() {
    let clock = Arc::new(SimulatedClock::new());
    let m = Arc::new(FakeModel::from_items([
        ScriptItem::Error(AgentError::Agent("overloaded".into())),
        ScriptItem::Spec(json!({"text": "ok"})),
    ]));
    let mut s = session(&logic(&m), &clock);
    let first = s.run(&RunInput::prompt("hi")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first.final_state, "toolLoop.timed_out");
    assert_eq!(first.context["last_failure"], "AgentError");
    assert_eq!(first.context["attempt"], 1);
    s.interpreter_mut().advance(&clock, 1000.0).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.result().output, json!("ok"));

    let failing =
        Arc::new(FakeModel::from_items([ScriptItem::Error(AgentError::Agent("x".into()))]));
    let one_try = logic(&failing).retry(AgentRetry { max_attempts: 1, ..AgentRetry::default() });
    let mut s = session(&one_try, &clock);
    s.run(&RunInput::prompt("hi")).unwrap_or_else(|e| panic!("{e}"));
    s.interpreter_mut().advance(&clock, 1000.0).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.result().error, json!({"kind": "retries", "message": "retries exhausted"}));
}

#[test]
fn human_wait_escalates_after_its_deadline_and_survives_snapshots() {
    let clock = Arc::new(SimulatedClock::new());
    let m = model(vec![json!({"tool": "refund", "args": {"order_id": "A"}})]);
    let agent = logic(&m).timeouts(60.0, 10.0);
    let mut s = session(&agent, &clock);
    let waiting = s.run(&RunInput::prompt("refund A")).unwrap_or_else(|e| panic!("{e}"));
    assert!(waiting.waiting);
    assert_eq!(pending_approval(&waiting.context)[0]["name"], "refund");
    let snap = s.snapshot().unwrap_or_else(|e| panic!("{e}"));
    let machine = tool_loop();
    let built = agent.build(&machine).unwrap_or_else(|e| panic!("{e}"));
    let mut resumed = AgentSession::restore(
        machine,
        built,
        &snap,
        Arc::clone(&clock) as Arc<dyn xstate_core::Clock>,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    resumed.trace(Arc::new(AgentTrace::default()));
    let wrong = xstate_core::Event::with_payload("HUMAN_APPROVED", json!({"call_ids": ["forged"]}));
    let still = resumed
        .run(&RunInput { event: Some(wrong), ..RunInput::default() })
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(still.waiting, "an approval naming other ids is refused");
    resumed.interpreter_mut().advance(&clock, 10_000.0).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        resumed.result().error,
        json!({"kind": "human_timeout", "message": "no human decision in time"})
    );
}

#[test]
fn turn_level_denials() {
    let two = vec![
        json!({"tool_calls": [{"name": "lookup", "arguments": {"query": "a"}}, {"name": "lookup", "arguments": {"query": "b"}}]}),
    ];
    let res = run(&logic(&model(two)).max_tool_calls(1), "x");
    assert_eq!(res.error["message"], "2 tool calls in one turn (max_tool_calls=1)");
    assert_eq!(res.context["pending_tool_calls"], json!([]));

    let dup = vec![
        json!({"tool_calls": [{"id": "same", "name": "lookup", "arguments": {"query": "a"}}, {"id": "same", "name": "lookup", "arguments": {"query": "b"}}]}),
    ];
    assert_eq!(run(&logic(&model(dup)), "x").error["message"], "duplicate tool call ids");

    let unknown = vec![json!({"tool": "rm_rf", "args": {}})];
    assert_eq!(
        run(&logic(&model(unknown)), "x").error["message"],
        "tool(s) ['rm_rf'] not allowed here"
    );

    let mut chart = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    chart["states"]["awaiting_model"]["meta"]["tools"] = json!(["lookup"]);
    let m = model(vec![json!({"tool": "refund", "args": {"order_id": "A"}})]);
    let res = run_agent(
        &logic(&m),
        RunOptions { chart: Some(chart), input: RunInput::prompt("x"), ..RunOptions::default() },
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .result;
    assert_eq!(res.error["message"], "tool(s) ['refund'] not allowed here");
    assert_eq!(
        m.calls()[0]["tools"],
        json!(["lookup"]),
        "the model is only told about allowed tools"
    );
}

#[test]
fn run_tool_rechecks_approval_when_the_chart_drops_the_guard() {
    let mut chart = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    let on_done = chart["states"]["awaiting_model"]["invoke"]["onDone"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("onDone"));
    on_done.retain(|t| t["guard"] != "needsHuman");
    let trace = Arc::new(AgentTrace::default());
    let m = model(vec![json!({"tool": "refund", "args": {"order_id": "A"}})]);
    let agent = logic(&m).tracer(Arc::clone(&trace));
    let res = run_agent(
        &agent,
        RunOptions { chart: Some(chart), input: RunInput::prompt("x"), ..RunOptions::default() },
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .result;
    assert_eq!(
        res.error,
        json!({"kind": "tool_denied", "message": "tool 'refund' denied: side_effect tool without human approval"})
    );
    let denied: Vec<Value> =
        trace.records().into_iter().filter(|r| r["kind"] == "tool_denied").collect();
    assert_eq!(denied[0]["reason"], "side_effect tool without human approval");
}

#[test]
fn tool_failures_and_timeouts_go_to_timed_out() {
    let res = run(&logic(&model(vec![json!({"tool": "slow"})])), "x");
    assert_eq!(res.final_state, "toolLoop.timed_out");
    assert_eq!(res.context["last_failure"], "ToolTimeoutError");
    let res = run(&logic(&model(vec![json!({"tool": "failing"})])), "x");
    assert_eq!(res.context["last_failure"], "ToolError");
}

#[test]
fn budgets_refuse_the_next_turn() {
    let script =
        || vec![json!({"tool": "lookup", "args": {"query": "q"}}), json!({"text": "done"})];
    let tokens = Budget::new(Some(15), None, None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        run(&logic(&model(script())).budget(tokens), "x").error["message"],
        "token budget exhausted"
    );
    let cost = Budget::new(None, Some(0.5), None).unwrap_or_else(|e| panic!("{e}"));
    let pricey = vec![
        json!({"tool": "lookup", "args": {"query": "q"}, "usage": {"cost_usd": 0.5}}),
        json!({"text": "done"}),
    ];
    assert_eq!(
        run(&logic(&model(pricey)).budget(cost), "x").error["message"],
        "cost budget exhausted"
    );
    let hostile = vec![
        json!({"tool": "lookup", "args": {"query": "q"}, "usage": {"input_tokens": -1000}}),
        json!({"text": "done"}),
    ];
    let res = run(&logic(&model(hostile)).budget(tokens), "x");
    assert_eq!(res.context["tokens_in"], 10, "negative usage never refunds a budget");
}

#[test]
fn system_prompt_reaches_the_model_but_not_the_context() {
    let m = model(vec![json!({"text": "hi"})]);
    let res = run(&logic(&m).system_prompt("be brief"), "hello");
    assert_eq!(m.calls()[0]["messages"][0], json!({"role": "system", "content": "be brief"}));
    assert_eq!(res.context["messages"][0]["role"], "user");
    assert_eq!(res.usage()["turns"], 1);
    assert!(!res.failed());
}

#[test]
fn spawned_tasks_start_without_start_and_bad_charts_are_refused() {
    let mut chart = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    chart["context"]["task"] = json!("from context");
    let m = model(vec![json!({"text": "ok"})]);
    let res = run_agent(&logic(&m), RunOptions { chart: Some(chart), ..RunOptions::default() })
        .unwrap_or_else(|e| panic!("{e}"))
        .result;
    assert_eq!(res.context["messages"][0]["content"], "from context");

    let mut typo = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    typo["states"]["awaiting_tool"]["meta"]["tools"] = json!(["lokup"]);
    let err =
        run_agent(&logic(&m), RunOptions { chart: Some(typo), ..RunOptions::default() }).err();
    assert!(err.is_some_and(|e| e.to_string().contains("'lokup'")));
}
