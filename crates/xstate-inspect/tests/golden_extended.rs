//! Extended golden coverage beyond the Python byte-for-byte cases: `after` (delayed transitions)
//! and invoke completion/error events (`done.invoke` / `error.platform`), which the `.jsonl`
//! golden generator cannot drive (it only calls `send`; it never advances a clock or registers a
//! failing service). These assert the protocol shapes the Rust observer emits for those
//! constructs; the child-actor `children` field and a real browser check are noted as untested in
//! the QA report (the sync observer has no live child registry and this suite is headless).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::Value;
use xstate_core::{MachineLogic, SimulatedClock, SyncInterpreter};
use xstate_inspect::{InspectorObserver, MemorySink};

fn kinds(sink: &MemorySink) -> Vec<String> {
    sink.messages().iter().map(|m| m["type"].as_str().unwrap_or_default().to_owned()).collect()
}

fn event_types(sink: &MemorySink) -> Vec<String> {
    sink.messages()
        .iter()
        .map(|m| m["event"]["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn after_delay_emits_the_python_spelled_event_and_a_snapshot() {
    let machine = xstate_core::Machine::from_json(
        r#"{"id":"t","initial":"a","states":{
            "a":{"after":{"1000":"b"}},
            "b":{"type":"final"}}}"#,
    )
    .unwrap();
    let sink = Arc::new(MemorySink::new());
    let observer = InspectorObserver::with_shared_sink(sink.clone()).clock(|| 7);
    let clock = Arc::new(SimulatedClock::new());
    let mut interp =
        SyncInterpreter::with_clock(machine, MachineLogic::dynamic(), Arc::clone(&clock) as _);
    interp.observe(Arc::new(observer));
    interp.start().unwrap();
    assert_eq!(interp.state_ids(), ["t.a"]);
    interp.advance(&clock, 1000.0).unwrap();
    assert_eq!(interp.state_ids(), ["t.b"]);

    // The event is the Python/v4 spelling `after.<delay>.<state>`.
    let types = event_types(&sink);
    assert!(types.contains(&"after.1000.t.a".to_owned()), "{types:?}");
    // actor, init event, init snapshot, after event, after snapshot
    assert_eq!(
        kinds(&sink),
        ["@xstate.actor", "@xstate.event", "@xstate.snapshot", "@xstate.event", "@xstate.snapshot"]
    );
}

#[test]
fn invoke_on_done_and_on_error_emit_done_invoke_and_error_platform() {
    let machine = xstate_core::Machine::from_json(
        r#"{"id":"load","initial":"fetching","states":{
            "fetching":{"invoke":{"src":"fetch","id":"fetcher","onDone":"done","onError":"failed"}},
            "done":{"type":"final"},
            "failed":{"type":"final"}}}"#,
    )
    .unwrap();
    let sink = Arc::new(MemorySink::new());
    let observer = InspectorObserver::with_shared_sink(sink.clone()).clock(|| 7);

    // Success: the service returns Ok, so `done.invoke.fetcher` fires and the machine reaches done.
    let logic = MachineLogic::dynamic().service("fetch", |_, input| Ok(input.clone()));
    let mut interp = SyncInterpreter::new(machine.clone(), logic);
    interp.observe(Arc::new(observer));
    interp.start().unwrap();
    assert_eq!(interp.state_ids(), ["load.done"]);
    let types = event_types(&sink);
    assert!(types.contains(&"done.invoke.fetcher".to_owned()), "{types:?}");

    // Failure: the service errors, so `error.platform.fetcher` fires instead.
    let sink2 = Arc::new(MemorySink::new());
    let observer2 = InspectorObserver::with_shared_sink(sink2.clone()).clock(|| 7);
    let failing =
        MachineLogic::dynamic().service("fetch", |_, _| Err(xstate_core::Error::logic("boom")));
    let mut interp = SyncInterpreter::new(machine, failing);
    interp.observe(Arc::new(observer2));
    interp.start().unwrap();
    let types = event_types(&sink2);
    assert!(types.contains(&"error.platform.fetcher".to_owned()), "{types:?}");
}

#[test]
fn allow_listed_payloads_are_redacted_in_place() {
    let machine = xstate_core::Machine::from_json(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#,
    )
    .unwrap();
    let sink = Arc::new(MemorySink::new());
    let observer = InspectorObserver::with_shared_sink(sink.clone())
        .include_payloads(true)
        .payload_allowlist(["nested", "note"]);
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.observe(Arc::new(observer));
    interp.start().unwrap();
    let payload = serde_json::json!({
        "nested": {"token": "t-123", "ok": [1, {"cvv": "9"}]},
        "note": "plain",
        "secret": "hunter2"
    });
    interp.send(xstate_core::Event::with_payload("GO", payload)).unwrap();
    let sent = sink.messages().into_iter().find(|m| m["event"]["type"] == "GO").unwrap();
    let event = &sent["event"];
    assert_eq!(event["note"], "plain", "allow-listed scalar passes through");
    assert_eq!(event.get("secret"), None, "non-allow-listed key is dropped");
    let nested = &event["nested"];
    assert!(nested.to_string().contains("***"), "nested secrets are redacted: {nested}");
    assert!(
        !nested.to_string().contains("hunter2") && !nested.to_string().contains("t-123"),
        "{nested}"
    );
    let _: Value = nested.clone();
}
