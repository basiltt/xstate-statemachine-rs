//! Test helpers for [`xstate_core`] machines: stub logic that records executed actions,
//! simulated-clock fixtures, path walking and statechart coverage. Phase 0 ships only the
//! crate skeleton.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub use xstate_core::SNAPSHOT_VERSION;
