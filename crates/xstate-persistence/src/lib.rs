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
#[cfg(feature = "postgres")]
pub mod postgres_backend;
pub mod receipts;
#[cfg(feature = "redis")]
pub mod redis_backend;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;
pub mod store;
pub mod timers;

pub use adopt::from_state_ids;
pub use file_store::{FileStore, FileStoreOptions};
#[cfg(feature = "sqlite")]
pub use idempotency::SqliteInbox;
pub use idempotency::{
    Admission, Claim, DEFAULT_TTL_S, IdempotencyGuard, InboxEntry, InboxErrorPolicy, InboxStore,
    MemoryInbox, fingerprint, scope, validate_idempotency_key, validate_principal,
};
pub use locking::{
    LockStrategy, NoLock, OptimisticLock, PersistedOptions, PessimisticLock, Session,
    VersionMismatch, load_interpreter, persisted, persisted_retry, save_interpreter,
};
#[cfg(feature = "sqlite")]
pub use log::SqliteLog;
pub use log::{
    Disposition, JsonLinesLog, MemoryLog, Origin, TransitionLogObserver, TransitionLogStore,
    TransitionRecord,
};
pub use memory_store::MemoryStore;
pub use migration::{MigrationError, SnapshotMigrator};
#[cfg(feature = "postgres")]
pub use postgres_backend::{
    DEFAULT_LOCK_TTL, PostgresInbox, PostgresLog, PostgresStore, PostgresStoreOptions,
};
pub use receipts::{ReceiptError, ReceiptErrorKind, ReceiptFlags, ReceiptJson, receipt_to_status};
#[cfg(feature = "redis")]
pub use redis_backend::{DEFAULT_LOCK_TTL_MS, RedisInbox, RedisLog, RedisStore, RedisStoreOptions};
#[cfg(feature = "sqlite")]
pub use sqlite_store::{JournalMode, SqliteStore, SqliteStoreOptions};
pub use store::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ForgetReport, Health, LockGuard, MAX_KEY_LEN, Result, SaveOptions,
    StateStore, StoreError, StoredSnapshot, validate_key,
};
pub use timers::{DueTimerScanner, ScanResult};
