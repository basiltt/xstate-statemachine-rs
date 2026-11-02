//! Multi-agent helpers on the sync engine: inline sub-agents, the global budget and handoffs.

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_agents::{
    AgentLogic, AgentTrace, Budget, BudgetPlugin, FakeModel, ModelCall, SpawnAgentConfig,
    ToolRegistry, ToolSpec, TraceIdentity, check_budget_event_declared, handoff_guard, load_chart,
    spawn_agent,
};
use xstate_core::{ActionScope, DynContext, Event, Machine, MachineLogic, SyncInterpreter};

fn parent_chart() -> Value {
    json!({
        "id": "sup", "initial": "working",
        "context": {"reports": [], "total_usage": {}, "usage_by_agent": {}, "budget_exceeded": false},
        "states": {
            "working": {"on": {
                "HANDOFF": {"guard": "handoffAllowed", "actions": "spawnWorker"},
                "AGENT_DONE": {"actions": "collect"},
                "AGENT_FAILED": {"actions": "collect"},
                "BUDGET_EXCEEDED": "stopped"
            }},
            "stopped": {"type": "final"}
        }
    })
}

fn search_tools() -> ToolRegistry {
    ToolRegistry::builder()
        .tool(ToolSpec::new("search", |_| Ok(json!("found"))))
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
}

fn worker(script: Vec<Value>, trace: &Arc<AgentTrace>) -> AgentLogic {
    let model: Arc<dyn ModelCall> = Arc::new(FakeModel::new(script));
    AgentLogic::new(model).tools(search_tools()).tracer(Arc::clone(trace))
}

fn parent(spawner: SpawnAgentConfig, plugin: &BudgetPlugin) -> SyncInterpreter<DynContext> {
    let machine = Machine::from_value(&parent_chart()).unwrap_or_else(|e| panic!("{e}"));
    let spawn = Arc::new(spawn_agent(spawner).unwrap_or_else(|e| panic!("{e}")));
    assert_eq!(spawn.action_name(), "spawnWorker");
    let logic = spawn.apply(MachineLogic::dynamic()).action(
        "collect",
        |s: &mut ActionScope<'_, DynContext>| {
            let report = json!({"type": s.event.ty.as_str(), "payload": s.event.payload.clone()});
            if let Some(list) = s.context["reports"].as_array_mut() {
                list.push(report);
            }
            Ok(())
        },
    );
    let logic = handoff_guard(plugin.guards(logic), "handoffAllowed", [("planner", ["worker"])]);
    let mut interp = SyncInterpreter::new(machine, logic);
    interp.observe(Arc::new(plugin.clone()));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    interp
}

fn handoff(from: &str, task: &str) -> Event {
    Event::with_payload("HANDOFF", json!({"from": from, "to": "worker", "task": task}))
}

#[test]
fn sub_agents_run_inline_and_report_with_usage() {
    let trace = Arc::new(AgentTrace::default());
    let script = vec![
        json!({"tool": "search", "args": {}}),
        json!({"text": "answer one"}),
        json!({"text": "answer two"}),
    ];
    let mut config = SpawnAgentConfig::new("worker", worker(script, &trace), Budget::default());
    config.parent_tools = Some(vec!["search".into(), "fetch".into()]);
    config.parent = TraceIdentity::root("sup");
    let plugin = BudgetPlugin::new(None, Some(1000)).unwrap_or_else(|e| panic!("{e}"));
    config.budget_plugin = Some(Arc::new(plugin.clone()));
    let mut sup = parent(config, &plugin);

    sup.send(handoff("planner", "first task")).unwrap_or_else(|e| panic!("{e}"));
    sup.send(handoff("planner", "second task")).unwrap_or_else(|e| panic!("{e}"));
    let refused = sup.send(handoff("stranger", "sneaky")).unwrap_or_else(|e| panic!("{e}"));
    assert!(refused.unhandled, "an unlisted handoff is refused");

    let ctx = sup.context().cloned().unwrap_or_default();
    let reports = ctx["reports"].as_array().cloned().unwrap_or_default();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["type"], "AGENT_DONE");
    assert_eq!(reports[0]["payload"]["agent_id"], "worker-1");
    assert_eq!(reports[0]["payload"]["result"], "answer one");
    assert_eq!(reports[0]["payload"]["usage"]["turns"], 2);
    assert_eq!(reports[1]["payload"]["agent_id"], "worker-2");
    assert_eq!(
        ctx["total_usage"],
        json!({"turns": 3, "input_tokens": 30, "output_tokens": 15, "cost_usd": 0.0})
    );
    assert_eq!(ctx["usage_by_agent"]["worker-2"]["turns"], 1);
    let model_calls: Vec<Value> =
        trace.records().into_iter().filter(|r| r["kind"] == "model_call").collect();
    assert_eq!(model_calls[0]["parent_id"], "sup");
    assert_eq!(model_calls[0]["trace_id"], "sup");
    assert_eq!(trace.totals()["agents"]["worker-1"]["turns"], 2);
}

#[test]
fn the_global_budget_trips_once_and_stops_spawning() {
    let trace = Arc::new(AgentTrace::default());
    let script = vec![json!({"text": "a"}), json!({"text": "b"})];
    let mut config = SpawnAgentConfig::new("worker", worker(script, &trace), Budget::default());
    config.parent_tools = Some(vec!["*".into()]);
    config.budget_plugin =
        Some(Arc::new(BudgetPlugin::new(None, Some(15)).unwrap_or_else(|e| panic!("{e}"))));
    let plugin = BudgetPlugin::new(None, Some(15)).unwrap_or_else(|e| panic!("{e}"));
    let mut sup = parent(config, &plugin);
    sup.send(handoff("planner", "t1")).unwrap_or_else(|e| panic!("{e}"));
    let ctx = sup.context().cloned().unwrap_or_default();
    assert_eq!(ctx["budget_exceeded"], true);
    assert_eq!(
        ctx["reports"].as_array().map(Vec::len),
        Some(1),
        "the report that tripped is still collected"
    );
    assert_eq!(sup.state_ids(), ["sup.stopped"]);
}

#[test]
fn failed_and_waiting_children_report_failure() {
    let trace = Arc::new(AgentTrace::default());
    let tools = ToolRegistry::builder()
        .tool(ToolSpec::new("pay", |_| Ok(Value::Null)).side_effect(true))
        .build()
        .unwrap_or_else(|e| panic!("{e}"));
    let model: Arc<dyn ModelCall> = Arc::new(FakeModel::new([json!({"tool": "pay"})]));
    let logic = AgentLogic::new(model).tools(tools).tracer(trace);
    let mut config = SpawnAgentConfig::new("worker", logic, Budget::default());
    config.parent_tools = Some(vec!["pay".into()]);
    let plugin = BudgetPlugin::new(Some(10.0), None).unwrap_or_else(|e| panic!("{e}"));
    let mut sup = parent(config, &plugin);
    sup.send(handoff("planner", "pay it")).unwrap_or_else(|e| panic!("{e}"));
    let ctx = sup.context().cloned().unwrap_or_default();
    assert_eq!(ctx["reports"][0]["type"], "AGENT_FAILED");
    assert_eq!(ctx["reports"][0]["payload"]["error"]["kind"], "waiting");
    let missing_task =
        sup.send(Event::with_payload("HANDOFF", json!({"from": "planner", "to": "worker"})));
    assert!(missing_task.is_err(), "a handoff without a task fails loudly");
}

#[test]
fn construction_refuses_unsafe_wiring() {
    let trace = Arc::new(AgentTrace::default());
    let make = || worker(vec![], &trace);
    assert!(
        spawn_agent(SpawnAgentConfig::new("w", make(), Budget::default())).is_err(),
        "closed by default"
    );
    let mut narrow = SpawnAgentConfig::new("w", make(), Budget::default());
    narrow.parent_tools = Some(vec!["fetch".into()]);
    let err = spawn_agent(narrow).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        err.contains("sub-agent tools [\"search\"] are not in the parent's allow-list"),
        "{err}"
    );
    let mut silent = SpawnAgentConfig::new("w", make(), Budget::default());
    silent.parent_tools = Some(vec!["*".into()]);
    let mut chart = load_chart("tool_loop").unwrap_or_else(|e| panic!("{e}"));
    chart["states"]["done"]["entry"] = json!([]);
    chart["states"]["error"]["entry"] = json!([]);
    silent.chart = Some(chart);
    assert!(spawn_agent(silent).is_err(), "a child that never reports is refused");
    assert!(BudgetPlugin::new(None, None).is_err());
    assert!(BudgetPlugin::new(Some(f64::INFINITY), None).is_err());
}

#[test]
fn budget_plugin_rollup_sanitises_reports() {
    let plugin =
        BudgetPlugin::new(Some(1.0), None).unwrap_or_else(|e| panic!("{e}")).max_tracked_agents(1);
    let mut ctx = json!({});
    let bad =
        json!({"agent_id": "a", "usage": {"turns": -3, "input_tokens": "x", "cost_usd": 0.4}});
    assert!(!plugin.rollup(&mut ctx, &bad));
    assert!(plugin.rollup(&mut ctx, &json!({"agent_id": "b", "usage": {"cost_usd": 0.7}})));
    assert!(
        !plugin.rollup(&mut ctx, &json!({"agent_id": "c", "usage": {"cost_usd": 5.0}})),
        "trips only once"
    );
    assert_eq!(ctx["total_usage"]["turns"], 0);
    assert_eq!(
        ctx["usage_by_agent"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["c".to_owned()])
    );
    assert!(plugin.exceeded(&ctx["total_usage"]));
}

#[test]
fn strict_parents_must_declare_budget_exceeded() {
    let strict = Machine::from_value(
        &json!({"id": "p", "strict": true, "initial": "a", "states": {"a": {"on": {"GO": "a"}}}}),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(check_budget_event_declared(&strict).is_err());
    let ok = Machine::from_value(&json!({"id": "p", "strict": true, "initial": "a", "states": {"a": {"on": {"BUDGET_EXCEEDED": "a"}}}}))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(check_budget_event_declared(&ok).is_ok());
    let mut interp = SyncInterpreter::new(strict, MachineLogic::dynamic());
    interp.observe(Arc::new(BudgetPlugin::new(Some(1.0), None).unwrap_or_else(|e| panic!("{e}"))));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
}
