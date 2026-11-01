//! Core engine of `xstate-statemachine`: an [XState v5](https://stately.ai/docs/xstate) /
//! [W3C SCXML](https://www.w3.org/TR/scxml/) compatible statechart runtime for Rust.
//!
//! This crate owns the machine model, the JSON configuration parser and validator, the
//! sans-I/O step engine (see ADR-0004), the synchronous interpreter, the pure transition API
//! and the Python-compatible snapshot format (see ADR-0003). It performs no I/O and has no
//! async runtime dependency; the async interpreter lives in `xstate-tokio`.
//!
//! The crate is being built in phases. Phase 0 (this version) only fixes the workspace,
//! the quality gates and the shared constants; the engine arrives in Phase 1.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

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
