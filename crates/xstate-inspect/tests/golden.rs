//! Byte-for-byte comparison with recordings produced by the Python `InspectorPlugin`
//! (`tests/golden/*.jsonl`, regenerated with `conformance/tools/inspect_golden.py`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::Value;
use xstate_core::{Event, Machine, MachineLogic, SyncInterpreter};
use xstate_inspect::{InspectorObserver, MemorySink, encode_line};

const CLOCK_START_MS: i64 = 1_700_000_000_000;
const CLOCK_STEP_MS: i64 = 500;

fn golden_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn observer_for(case: &Value, sink: Arc<MemorySink>) -> InspectorObserver {
    let options = &case["options"];
    let ticks = AtomicI64::new(0);
    let mut observer = InspectorObserver::with_shared_sink(sink)
        .context_allowlist(string_list(options.get("context_allowlist")))
        .payload_allowlist(string_list(options.get("payload_allowlist")))
        .clock(move || CLOCK_START_MS + ticks.fetch_add(1, Ordering::Relaxed) * CLOCK_STEP_MS);
    if options.get("include_payloads").and_then(Value::as_bool) == Some(true) {
        observer = observer.include_payloads(true);
    }
    observer
}

/// Actions named in the case configs; they do nothing, like Python's `stub_logic`.
fn stub_logic(config: &Value) -> MachineLogic<xstate_core::DynContext> {
    let mut logic = MachineLogic::dynamic();
    if config.to_string().contains("bump") {
        logic = logic.action("bump", |_| Ok(()));
    }
    logic
}

fn run_case(case: &Value) -> Vec<String> {
    let machine = Machine::from_value(&case["config"]).unwrap_or_else(|e| panic!("{e}"));
    let sink = Arc::new(MemorySink::new());
    let observer = observer_for(case, sink.clone());
    let mut interp = SyncInterpreter::new(machine, stub_logic(&case["config"]));
    interp.observe(Arc::new(observer));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    for ev in case["events"].as_array().into_iter().flatten() {
        let ty = ev["type"].as_str().unwrap_or_default();
        let event = match ev.get("payload") {
            Some(p) => Event::with_payload(ty, p.clone()),
            None => Event::new(ty),
        };
        interp.send(event).unwrap_or_else(|e| panic!("{e}"));
    }
    interp.stop();
    sink.messages().iter().map(encode_line).collect()
}

#[test]
fn recordings_match_python_byte_for_byte() {
    let spec: Value = serde_json::from_str(
        &std::fs::read_to_string(golden_dir().join("cases.json")).unwrap_or_else(|e| panic!("{e}")),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let cases = spec["cases"].as_array().unwrap_or_else(|| panic!("cases.json has no cases"));
    assert_ne!(cases.len(), 0, "cases.json is empty");
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let expected = std::fs::read_to_string(golden_dir().join(format!("{name}.jsonl")))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected: Vec<&str> = expected.lines().collect();
        let actual = run_case(case);
        assert_eq!(actual.len(), expected.len(), "{name}: message count");
        for (i, (got, want)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(got, want, "{name}: message {i}");
        }
    }
}

#[test]
fn sink_failure_disables_the_observer_once() {
    let machine =
        Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#)
            .unwrap_or_else(|e| panic!("{e}"));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let failing = move |_: &Value| {
        counter.fetch_add(1, Ordering::Relaxed);
        Err(xstate_inspect::SinkError::Closed)
    };
    let observer = Arc::new(InspectorObserver::new(failing));
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(observer.clone());
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    interp.send("GO").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(calls.load(Ordering::Relaxed), 1, "reported once, then inert");
    assert!(observer.is_disabled());
    assert_eq!(interp.state_ids(), ["m.b"], "the machine is never affected");
}

#[test]
fn child_identity_fields_are_carried() {
    let machine = Machine::from_json(r#"{"id":"child","initial":"a","states":{"a":{}}}"#)
        .unwrap_or_else(|e| panic!("{e}"));
    let sink = Arc::new(MemorySink::new());
    let observer = InspectorObserver::with_shared_sink(sink.clone())
        .session_id("order:42:worker")
        .root_id("order:42")
        .parent_id("order:42")
        .name("worker")
        .clock(|| 7);
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(Arc::new(observer));
    interp.start().unwrap_or_else(|e| panic!("{e}"));
    let msgs = sink.messages();
    assert_eq!(msgs[0]["sessionId"], "order:42:worker");
    assert_eq!(msgs[0]["rootId"], "order:42");
    assert_eq!(msgs[0]["parentId"], "order:42");
    assert_eq!(msgs[0]["name"], "worker");
    assert_eq!(msgs[0]["createdAt"], "7");
    assert_eq!(msgs[1]["sourceId"], "order:42", "xstate.init comes from the parent");
}
