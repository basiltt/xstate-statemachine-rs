//! The transition / audit log: who did what, and why.
//!
//! An append-only record per processed event, written from the same observer hook that saw the
//! transition, so the log cannot disagree with the state. Ports the Python
//! `persistence/log.py`:
//!
//! * [`TransitionRecord`] with the Python field layout and JSON form (`json.dumps(...,
//!   sort_keys=True)`), so a JSONL file or a SQLite `transitions` table written by one runtime
//!   is readable by the other;
//! * [`TransitionLogStore`] with [`MemoryLog`], [`JsonLinesLog`] and (feature `sqlite`)
//!   [`SqliteLog`];
//! * [`TransitionLogObserver`], the Rust counterpart of `TransitionLogPlugin` / `AuditPlugin`
//!   (see its docs for the differences), with [`redact`] and [`DEFAULT_REDACT_KEYS`].
//!
//! # Not ported
//!
//! * `replay()` and `ReplayDivergenceError`: a documented TODO on [`TransitionLogObserver`].
//! * The `persisted()` commit seam (`buffer_marks` / `flush_marks` / `seal_marks`): records are
//!   appended when the event settles, not after the snapshot save, so after a failed save the
//!   log can lead the state. Callers that need Python's guarantee should buffer records
//!   themselves and append after the save.
//! * Joining the snapshot store's SQLite transaction (`SQLiteLog(store)`): the log owns its
//!   connection.
//! * The `correlation_id_var` context variable.

mod jsonl;
mod memory;
mod observer;
mod record;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use jsonl::JsonLinesLog;
pub use memory::MemoryLog;
pub use observer::{AuditKeys, DEFAULT_REDACT_KEYS, TransitionLogObserver, redact};
pub use record::{Disposition, Origin, TransitionLogStore, TransitionRecord};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteLog;
