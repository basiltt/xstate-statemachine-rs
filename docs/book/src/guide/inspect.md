# Stately Inspector

`xstate-inspect` (feature `inspect`) emits the `@statelyai/inspect` protocol (pinned to 0.7.2):
actor registration, events and snapshots. The message bytes are identical to the Python library's
`InspectorPlugin` — golden recordings written by Python are compared line by line with what the
Rust observer emits for the same run.

- `InspectorObserver` — an `Observer` you attach with `interp.observe(...)`. Context and event
  payloads are deny-by-default (allow-lists plus key-based redaction).
- Sinks: `MemorySink` for in-process collection, and `.jsonl` recording.
- `SseSink` — a hardened loopback SSE server that streams a live machine to the Stately Inspector
  page (`xsm inspect --live` is built on it). Non-loopback binds require a token.
- Replay: `xsm replay run.jsonl --live` feeds a recording back into the Inspector UI.

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::inspect::{InspectorObserver, MemorySink};

let machine = Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#)?;
let sink = Arc::new(MemorySink::new());
let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
interp.observe(Arc::new(InspectorObserver::with_shared_sink(sink.clone())));
interp.start()?;
interp.send("GO")?;
assert_eq!(sink.messages().len(), 5); // actor, event, snapshot, event, snapshot
# Ok::<(), Box<dyn std::error::Error>>(())
```

The axum integration also serves the Inspector over a token-gated WebSocket endpoint; see
[HTTP integration (axum)](axum.md) and the CLI (`xsm inspect --live`, `xsm replay`). The
security model (loopback binds, Host checks, token comparison) is on the
[security hardening](../operations/security.md) page.
