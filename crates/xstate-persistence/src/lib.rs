//! Persistence for [`xstate_core`] machines: the [`StateStore`] contract, in-process and
//! SQLite stores, optimistic / pessimistic locking with the [`persisted`] transaction wrapper,
//! durable `after` deadlines and their scanner, the idempotency inbox, the transition / audit
//! log, snapshot migrations and adoption of existing records.
//!
//! All on-disk layouts are byte-compatible with the Python library (ADR-0003): a SQLite file
//! or a directory of `.xsm.json` records can be shared between the two runtimes.

#![forbid(unsafe_code)]

pub mod adopt;
pub mod file_store;
pub mod idempotency;
pub mod locking;
pub mod log;
pub mod memory_store;
pub mod migration;
pub mod receipts;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;
pub mod store;
pub mod timers;

pub use store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, MAX_KEY_LEN, Result, SaveOptions,
    StateStore, StoreError, StoredSnapshot, validate_key,
};
