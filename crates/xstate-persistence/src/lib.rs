//! Persistence for [`xstate_core`] machines: the `StateStore` contract, memory/file/SQLite
//! stores, optimistic and pessimistic locking, the `persisted` transaction wrapper, durable
//! `after` deadlines and their scanner, the idempotency inbox, the transition/audit log and
//! snapshot migrations.
//!
//! All on-disk and on-wire layouts are byte-compatible with the Python library (ADR-0003).
//! Phase 0 ships only the crate skeleton.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub use xstate_core::SNAPSHOT_VERSION;
