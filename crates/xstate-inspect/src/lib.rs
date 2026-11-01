//! Stately Inspector support for [`xstate_core`] machines.
//!
//! * [`InspectorObserver`] — an [`xstate_core::Observer`] that turns interpreter activity into
//!   `@statelyai/inspect` protocol messages (actor registration, events, snapshots) with
//!   deny-by-default context / payload allow-lists and key-based redaction;
//! * [`protocol`] — the message builders (byte-compatible with the Python library's);
//! * [`sinks`] — [`MemorySink`], [`JsonLinesSink`] (`.jsonl` recordings readable by the
//!   Python `xsm inspect replay` and vice versa) and the [`Sink`] trait for custom transports;
//! * [`replay`] — replay a recording into any sink, optionally honouring recorded timing.
//!
//! ```
//! use std::sync::Arc;
//! use xstate_core::{Machine, MachineLogic, SyncInterpreter};
//! use xstate_inspect::{InspectorObserver, MemorySink};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let machine = Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#)?;
//! let sink = Arc::new(MemorySink::new());
//! let observer = InspectorObserver::with_shared_sink(sink.clone());
//! let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
//! interp.observe(Arc::new(observer));
//! interp.start()?;
//! interp.send("GO")?;
//! let kinds: Vec<_> = sink.messages().iter().map(|m| m["type"].as_str().unwrap_or("").to_owned()).collect();
//! assert_eq!(kinds, ["@xstate.actor", "@xstate.event", "@xstate.snapshot", "@xstate.event", "@xstate.snapshot"]);
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod observer;
pub mod protocol;
pub mod redact;
pub mod replay;
pub mod sinks;

pub use observer::InspectorObserver;
pub use protocol::{MESSAGE_TYPES, PROTOCOL_VERSION};
pub use redact::{DEFAULT_REDACT_KEYS, RedactKeys, redact};
pub use replay::{ReplayError, ReplayOptions, replay_file, replay_with};
pub use sinks::{JsonLinesSink, MemorySink, Sink, SinkError, encode_line, read_jsonl};
