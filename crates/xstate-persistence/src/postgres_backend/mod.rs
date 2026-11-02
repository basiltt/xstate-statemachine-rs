//! PostgreSQL backends for the stores of this crate (feature `postgres`): [`PostgresStore`],
//! [`PostgresInbox`] and [`PostgresLog`].
//!
//! They are ports of Python's `contrib/sqlalchemy` package on its PostgreSQL dialect and use the
//! identical tables (`xsm_snapshots` or a configured name, `xsm_deadlines`, `xsm_locks`,
//! `xsm_inbox`, `xsm_transitions`, `xsm_schema` with the `sqlalchemy` version row), so one
//! database can be shared by Rust and Python processes.
//!
//! They implement the synchronous store traits by driving sqlx on a tokio runtime with `block_on`
//! and plug into `persisted()` unchanged. **Never call them from a task running on the runtime
//! they use**; from async code go through `tokio::task::spawn_blocking`, like the other stores.
//! See [`PostgresStore`] for `connect` (own runtime) versus `from_pool` (your pool and runtime).
//!
//! # Deliberate differences from Python
//!
//! * No shared transaction: Python's `lock()` / `transaction()` make the store, inbox and log of
//!   one thread commit together. Here every call is its own transaction; the in-snapshot
//!   processed ring covers the gap between save and inbox mark, as for the other Rust backends.
//! * `lock()` is not re-entrant on one thread (Python's is).
//! * `list_keys` and `due_keys` order by code point (`COLLATE "C"`), not by the database's default
//!   collation, as the [`StateStore`](crate::StateStore) contract says.

mod db;
mod inbox;
mod log;
mod ops;
mod schema;
mod store;

pub use inbox::PostgresInbox;
pub use log::PostgresLog;
pub use schema::SCHEMA_VERSION;
pub use store::{DEFAULT_LOCK_TTL, PostgresStore, PostgresStoreOptions};
