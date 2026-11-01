//! Core engine of `xstate-statemachine`: an [XState v5](https://stately.ai/docs/xstate) /
//! [W3C SCXML](https://www.w3.org/TR/scxml/) compatible statechart runtime for Rust.
//!
//! Load a machine exported from the Stately editor as JSON, supply the named guards, actions
//! and services in a [`MachineLogic`] registry, and run it with the [`SyncInterpreter`]
//! (or the async interpreter in `xstate-tokio`).
//!
//! ```
//! use xstate_core::{Machine, MachineLogic, SyncInterpreter};
//!
//! let machine = Machine::from_json(r#"{
//!   "id": "toggle", "initial": "inactive",
//!   "states": { "inactive": { "on": { "TOGGLE": "active" } },
//!               "active":   { "on": { "TOGGLE": "inactive" } } }
//! }"#)?;
//! let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
//! interp.start()?;
//! interp.send("TOGGLE")?;
//! assert_eq!(interp.state_ids(), ["toggle.active"]);
//! # Ok::<(), xstate_core::Error>(())
//! ```
//!
//! The crate owns the machine model ([`machine`]), the JSON parser ([`config`]), the
//! sans-I/O step engine ([`engine`], ADR-0004), the synchronous interpreter and the clocks.
//! It performs no I/O and has no async runtime dependency.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod builder;
pub mod clock;
pub mod config;
pub mod context;
pub mod engine;
mod error;
pub mod event;
pub mod export;
mod ids;
mod interpreter;
pub mod logic;
pub mod machine;
pub mod pure;
mod pyjson;
pub mod snapshot;

pub use clock::{Clock, RealClock, SimulatedClock};
pub use context::{Context, DynContext};
pub use engine::{ActiveState, Effect, Status, StepReport};
pub use error::{Error, ErrorKind, Result};
pub use event::{Event, EventOrigin};
pub use ids::{StateId, TransitionId};
pub use interpreter::{Receipt, RestartTimers, RestoreOptions, SyncInterpreter};
pub use logic::{ActionScope, MachineLogic};
pub use machine::Machine;
pub use snapshot::Snapshot;

/// Snapshot layout version written and accepted by this engine.
///
/// The layout is byte-compatible with the Python library's `SNAPSHOT_VERSION` so that a
/// Python-written snapshot can be resumed by a Rust worker and vice versa (ADR-0003).
///
/// ```
/// assert_eq!(xstate_core::SNAPSHOT_VERSION, 4);
/// ```
pub const SNAPSHOT_VERSION: u32 = 4;

/// Default `maxIterations` budget for eventless (`always`) settling and self-generated event
/// chains, matching XState v5.31 and the Python engine.
pub const DEFAULT_MAX_ITERATIONS: u32 = 1000;

/// Default upper bound, in bytes, for configuration, event and snapshot payloads parsed from
/// untrusted sources (1 MiB, as in the Python library's security model).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
