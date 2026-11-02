# xstate-inspect

Stately Inspector support for `xstate-core` machines: an observer that emits
`@statelyai/inspect` protocol messages (actor registration, events, snapshots), sinks for
in-memory collection and `.jsonl` recording, replay of recordings, and a hardened loopback
SSE server that streams them to the Stately Inspector page.

Context and event payloads are deny-by-default (allow-lists plus key-based redaction), and the
message bytes are identical to the Python library's `InspectorPlugin`, so recordings made by
either runtime replay in the other and in the Stately Inspector UI.

Status: 0.1.0. Protocol pinned to `@statelyai/inspect` 0.7.2; `xsm inspect --live` and
`xsm replay` are built on it.

```rust
use std::sync::Arc;
use xstate_core::{Machine, MachineLogic, SyncInterpreter};
use xstate_inspect::{InspectorObserver, MemorySink};

let machine = Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#)?;
let sink = Arc::new(MemorySink::new());
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.observe(Arc::new(InspectorObserver::with_shared_sink(sink.clone())));
interp.start()?;
interp.send("GO")?;
assert_eq!(sink.messages().len(), 5); // actor, event, snapshot, event, snapshot
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

None; the SSE server uses the standard library only.

## Python counterpart

[`xstate_statemachine.inspect`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/inspect)
(`plugin`, `protocol`, `sinks`, `replay`). The axum WebSocket endpoint lives in
[`xstate-axum`](../xstate-axum).
