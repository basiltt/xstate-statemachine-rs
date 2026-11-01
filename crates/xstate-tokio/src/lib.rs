//! Asynchronous interpreter for [`xstate_core`] machines on the tokio runtime.
//!
//! The interpreter drives the sans-I/O step engine (ADR-0004): it owns the mailbox, maps
//! `ArmTimer`/`StartInvoke`/`SendTo` effects onto `tokio::time` and tasks, and hosts invoked
//! services and spawned child actors. Phase 0 ships only the crate skeleton.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub use xstate_core::SNAPSHOT_VERSION;
