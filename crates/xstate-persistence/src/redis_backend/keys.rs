//! Key names under one validated prefix, shared by the store, inbox and log.
//!
//! Python reference: `contrib/redis/_keys.py`. The layout is, for `{p}` the prefix:
//!
//! ```text
//! {p}:schema            string  layout version
//! {p}:keys              set     every snapshot key (for list_keys)
//! {p}:snap:{key}        hash    snapshot, version, machine_version, updated_at
//! {p}:dl:{key}          hash    field = zset member -> JSON deadline
//! {p}:deadlines         zset    member JSON ["key","state_id",entry_seq,"event"],
//!                               scored by due_at_wall (the scanner index)
//! {p}:lock:{key}        string  random token, PX ttl
//! {p}:inbox:{scope}     hash    field key -> JSON inbox entry
//! {p}:inbox_exp         zset    member "{len(scope)}:{scope}{key}" scored by expires_at
//! {p}:log:{machine_id}  stream  one entry per transition record, id "{seq}-0"
//! ```

use crate::store::{Result, StoreError};

/// The layout version this library writes (Python `SCHEMA_VERSION`). Version 2 made the
/// deadline members JSON arrays; version 1 members (`"{key}|{field}"`) are still read and
/// cleaned up.
pub(crate) const SCHEMA_VERSION: i64 = 2;

/// Characters that are special in a `SCAN` / `SSCAN` glob.
const GLOB_META: [char; 5] = ['*', '?', '[', ']', '\\'];

/// Escape `text` so a glob matches it literally (Python `escape_glob`).
pub(crate) fn escape_glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if GLOB_META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Every Redis key name of one namespace.
#[derive(Debug, Clone)]
pub(crate) struct Keys {
    prefix: String,
}

impl Keys {
    /// Validate `prefix` (non-empty, no whitespace) and strip trailing colons (Python
    /// `validate_prefix`): two applications sharing a Redis must never collide by default.
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        if prefix.trim().is_empty() {
            return Err(StoreError::Other(
                "Redis backends need a non-empty `prefix` (the key namespace); two applications \
                 sharing one Redis must never collide by default"
                    .into(),
            ));
        }
        if prefix.contains([' ', '\t', '\n', '\r']) {
            return Err(StoreError::Other("Redis `prefix` must not contain whitespace".into()));
        }
        let trimmed = prefix.trim_end_matches(':');
        if trimmed.is_empty() {
            return Err(StoreError::Other("Redis `prefix` must not consist of colons only".into()));
        }
        Ok(Self { prefix: trimmed.to_owned() })
    }

    /// The prefix as stored (trailing colons stripped).
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn schema(&self) -> String {
        format!("{}:schema", self.prefix)
    }

    pub(crate) fn keys(&self) -> String {
        format!("{}:keys", self.prefix)
    }

    pub(crate) fn snap(&self, key: &str) -> String {
        format!("{}:snap:{key}", self.prefix)
    }

    pub(crate) fn dl(&self, key: &str) -> String {
        format!("{}:dl:{key}", self.prefix)
    }

    pub(crate) fn deadlines(&self) -> String {
        format!("{}:deadlines", self.prefix)
    }

    pub(crate) fn lock(&self, key: &str) -> String {
        format!("{}:lock:{key}", self.prefix)
    }

    pub(crate) fn inbox(&self, scope: &str) -> String {
        format!("{}:inbox:{scope}", self.prefix)
    }

    pub(crate) fn inbox_exp(&self) -> String {
        format!("{}:inbox_exp", self.prefix)
    }

    pub(crate) fn log(&self, machine_id: &str) -> String {
        format!("{}:log:{machine_id}", self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_rules() {
        assert_eq!(Keys::new("app::").unwrap().prefix(), "app");
        assert!(Keys::new("").is_err());
        assert!(Keys::new("  ").is_err());
        assert!(Keys::new("a b").is_err());
        assert!(Keys::new("::").is_err());
    }

    #[test]
    fn key_names_follow_the_python_layout() {
        let k = Keys::new("app").unwrap();
        assert_eq!(k.snap("order:1"), "app:snap:order:1");
        assert_eq!(k.dl("x"), "app:dl:x");
        assert_eq!(k.deadlines(), "app:deadlines");
        assert_eq!(k.inbox("p/m/i"), "app:inbox:p/m/i");
        assert_eq!(k.inbox_exp(), "app:inbox_exp");
        assert_eq!(k.log("m"), "app:log:m");
        assert_eq!(k.lock("x"), "app:lock:x");
        assert_eq!(k.schema(), "app:schema");
        assert_eq!(k.keys(), "app:keys");
    }

    #[test]
    fn glob_metacharacters_are_escaped() {
        assert_eq!(escape_glob("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
        assert_eq!(escape_glob("plain"), "plain");
    }
}
