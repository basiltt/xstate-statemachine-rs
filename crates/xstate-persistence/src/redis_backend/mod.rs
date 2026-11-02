//! Redis backends for the stores of this crate (feature `redis`): [`RedisStore`],
//! [`RedisInbox`] and [`RedisLog`].
//!
//! They are ports of Python's `contrib/redis` package and write the identical layout (key names,
//! hash fields, JSON encodings, deadline index, lock tokens, Lua scripts), so one
//! Redis namespace can be shared by Rust and Python processes. They implement the synchronous
//! store traits over blocking connections and plug into `persisted()` unchanged; call them
//! from async code through `spawn_blocking` like the other stores.

mod conn;
mod deadlines;
mod inbox;
mod keys;
mod log;
mod scripts;
mod store;

pub use inbox::RedisInbox;
pub use log::RedisLog;
pub use store::{DEFAULT_LOCK_TTL_MS, RedisStore, RedisStoreOptions};
