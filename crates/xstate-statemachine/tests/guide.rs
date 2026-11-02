//! The code of `docs/guide/*.md`, compiled and run so the guide cannot rot. Each test names the
//! page it mirrors; the snippets in the pages are these bodies without the test scaffolding.

use xstate_statemachine::prelude::*;

// ------------------------------------------------------------------------ getting-started.md

#[test]
fn getting_started_door() -> Result<(), Box<dyn std::error::Error>> {
    let machine = Machine::from_json(
        r#"{
      "id": "door", "initial": "closed", "context": {"opens": 0, "locked": false},
      "states": {
        "closed": {"on": {"OPEN": {"target": "opened", "guard": "isUnlocked", "actions": "countOpen"}}},
        "opened": {"on": {"CLOSE": "closed"}}
      }
    }"#,
    )?;
    let logic = MachineLogic::dynamic()
        .guard("isUnlocked", |ctx, _event, _params| Ok(ctx["locked"] != true))
        .action("countOpen", |scope| {
            let n = scope.context["opens"].as_u64().unwrap_or(0) + 1;
            scope.context["opens"] = n.into();
            Ok(())
        });
    let mut door = SyncInterpreter::new(machine, logic);
    door.start()?;
    door.send("OPEN")?;
    assert_eq!(door.state_ids(), ["door.opened"]);
    assert_eq!(door.context().map(|c| c["opens"].clone()), Some(1.into()));
    door.send("BANANA")?; // not declared anywhere: ignored
    assert_eq!(door.state_ids(), ["door.opened"]);
    Ok(())
}

#[test]
fn getting_started_missing_logic_is_reported_at_start() {
    let machine = Machine::from_json(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":{"target":"b","guard":"nope"}}},"b":{}}}"#,
    )
    .unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    let err = interp.start().unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "current_thread")]
async fn getting_started_async_actor() -> Result<(), Box<dyn std::error::Error>> {
    use xstate_statemachine::tokio::Options;

    let machine = Machine::from_json(
        r#"{"id":"fetch","initial":"idle","states":{
            "idle":{"on":{"FETCH":"loading"}},
            "loading":{"invoke":{"src":"load","onDone":"done","onError":"failed"}},
            "done":{},"failed":{}}}"#,
    )?;
    let logic = MachineLogic::dynamic()
        .async_service("load", |_ctx, _input| Box::pin(async { Ok(serde_json::json!("ok")) }));
    let actor = spawn(machine, logic, Options::default())?;
    let mut view = actor.subscribe();
    actor.send("FETCH").await?;
    view.wait_for(|v| v.state_ids == ["fetch.done"]).await?;
    actor.stop().await;
    Ok(())
}

// --------------------------------------------------------------------- stately-editor.md

const EXPORTED: &str = r#"{
  "id": "checkout",
  "initial": "cart",
  "context": {"items": 0},
  "types": {"events": {}},
  "states": {
    "cart": {"on": {"ADD": {"actions": "addItem"},
                    "PAY": {"target": "paying", "guard": "hasItems"}}},
    "paying": {
      "after": {"30000": "expired"},
      "on": {"PAID": "done", "FAILED": "cart"}
    },
    "expired": {"always": {"target": "cart"}},
    "done": {"type": "final"}
  }
}"#;

#[test]
fn stately_editor_export_runs() -> Result<(), Box<dyn std::error::Error>> {
    let machine = Machine::from_json(EXPORTED)?;
    assert!(machine.warnings().is_empty(), "{:?}", machine.warnings());
    let logic = MachineLogic::dynamic()
        .guard("hasItems", |ctx, _e, _p| Ok(ctx["items"].as_u64().unwrap_or(0) > 0))
        .action("addItem", |scope| {
            let n = scope.context["items"].as_u64().unwrap_or(0) + 1;
            scope.context["items"] = n.into();
            Ok(())
        });
    let clock = std::sync::Arc::new(xstate_statemachine::SimulatedClock::new());
    let mut checkout = SyncInterpreter::with_clock(machine, logic, clock.clone());
    checkout.start()?;
    checkout.send("PAY")?; // guard false: stays in the cart
    assert_eq!(checkout.state_ids(), ["checkout.cart"]);
    checkout.send("ADD")?;
    checkout.send("PAY")?;
    assert_eq!(checkout.state_ids(), ["checkout.paying"]);
    checkout.advance(&clock, 31_000.0)?;
    assert_eq!(checkout.state_ids(), ["checkout.cart"]);
    Ok(())
}

// ------------------------------------------------------------------------- persistence.md

#[cfg(feature = "sqlite")]
#[test]
fn persistence_sqlite_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    use xstate_statemachine::persistence::{PersistedOptions, SqliteStore};

    let machine = Machine::from_json(EXPORTED)?;
    let logic = MachineLogic::dynamic()
        .guard("hasItems", |ctx, _e, _p| Ok(ctx["items"].as_u64().unwrap_or(0) > 0))
        .action("addItem", |scope| {
            let n = scope.context["items"].as_u64().unwrap_or(0) + 1;
            scope.context["items"] = n.into();
            Ok(())
        });
    let store = SqliteStore::open_in_memory()?;
    let options = PersistedOptions::default();
    persisted(&store, "order-1", &machine, &logic, &options, |interp, _| {
        interp.send("ADD")?;
        Ok(())
    })?;
    // A later request, possibly another process (or a Python worker): restore, act, save.
    let state = persisted(&store, "order-1", &machine, &logic, &options, |interp, _| {
        interp.send("PAY")?;
        Ok(interp.state_ids())
    })?;
    assert_eq!(state, ["checkout.paying"]);
    Ok(())
}

#[cfg(feature = "persistence")]
#[test]
fn persistence_locks_and_retry() -> Result<(), Box<dyn std::error::Error>> {
    use xstate_statemachine::persistence::{
        MemoryStore, OptimisticLock, PersistedOptions, persisted_retry,
    };

    let machine = Machine::from_json(EXPORTED)?;
    let logic = MachineLogic::dynamic()
        .guard("hasItems", |_ctx, _e, _p| Ok(true))
        .action("addItem", |_scope| Ok(()));
    let store = MemoryStore::new();
    let options = PersistedOptions {
        lock: Box::new(OptimisticLock { retries: 3, ..OptimisticLock::default() }),
        ..PersistedOptions::default()
    };
    persisted_retry(&store, "o-1", &machine, &logic, &options, |interp, session| {
        interp.send("ADD")?;
        session.after_commit(|| Ok(()));
        Ok(())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------------- eda.md

#[cfg(feature = "eda")]
mod eda {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use xstate_statemachine::eda::{
        AsyncApiOptions, Dispatch, Envelope, HandlerError, InboundDispatcher,
        MemoryDeadLetterStore, SyncFakeBrokerAdapter, asyncapi_document, machines_by_type,
    };
    use xstate_statemachine::persistence::receipts::ReceiptFlags;
    use xstate_statemachine::persistence::{
        MemoryInbox, MemoryStore, PersistedOptions, ReceiptJson, persisted,
    };
    use xstate_statemachine::prelude::*;

    const COUNTER: &str = r#"{"id":"counter","initial":"idle","context":{"total":0},
        "states":{"idle":{"on":{"ADD":{"actions":"add"}}}}}"#;

    #[test]
    fn consume_envelopes_through_a_dispatcher() -> Result<(), Box<dyn std::error::Error>> {
        let machine = Machine::from_json(COUNTER)?;
        let logic = MachineLogic::dynamic().action("add", |scope| {
            let n = scope.event.payload["n"].as_i64().unwrap_or(0);
            let total = scope.context["total"].as_i64().unwrap_or(0) + n;
            scope.context["total"] = total.into();
            Ok(())
        });
        let store = Arc::new(MemoryStore::new());
        let handler = {
            let store = Arc::clone(&store);
            move |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
                let options = PersistedOptions::default();
                let receipt =
                    persisted(&*store, d.key, d.machine, &logic, &options, |interp, _| {
                        let r = interp.send(d.event.clone())?;
                        Ok(ReceiptJson::from_receipt(&r, ReceiptFlags::default(), None))
                    })?;
                HandlerError::check_receipt(receipt)
            }
        };
        let dispatcher = InboundDispatcher::new(
            machines_by_type(HashMap::from([("xsm.counter.ADD".to_owned(), machine)])),
            handler,
        )
        .inbox(Arc::new(MemoryInbox::new()))
        .max_attempts(3)?
        .dead_letters(Arc::new(MemoryDeadLetterStore::new()));

        let broker = SyncFakeBrokerAdapter::new();
        let envelope =
            Envelope::new("xsm.counter.ADD").subject("c-1").data(json!({"n": 5})).build()?;
        broker.deliver("counters", &envelope)?;
        broker.deliver("counters", &envelope)?; // redelivery: deduplicated through the inbox
        let result = dispatcher.run_once_sync(&broker, "counters", Duration::ZERO)?;
        assert_eq!((result.processed, result.duplicates), (1, 1));
        Ok(())
    }

    #[test]
    fn asyncapi_from_a_chart() -> Result<(), Box<dyn std::error::Error>> {
        let machine = Machine::from_json(COUNTER)?;
        let doc = asyncapi_document(&machine, &AsyncApiOptions::default())?;
        assert_eq!(doc["asyncapi"], "3.0.0");
        Ok(())
    }
}

// ------------------------------------------------------------------------------ features.md

#[cfg(feature = "macros")]
#[test]
fn typed_dsl() -> Result<(), Box<dyn std::error::Error>> {
    #[derive(TypedEvent)]
    #[event(rename_all = "SCREAMING_SNAKE_CASE")]
    enum Event2 {
        Open,
        Close,
    }

    #[derive(Debug, Clone, Copy, PartialEq, TypedState)]
    #[state(machine = "door")]
    enum Door {
        Closed,
        Opened,
    }

    let machine = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opened } }
            opened { on { CLOSE => closed } }
        }
    }?;
    let mut door = SyncInterpreter::new(machine, MachineLogic::dynamic());
    door.start()?;
    door.send(Event2::Open)?;
    assert!(Door::Opened.is_active(&door));
    door.send(Event2::Close)?;
    assert!(Door::Closed.is_active(&door));
    Ok(())
}
