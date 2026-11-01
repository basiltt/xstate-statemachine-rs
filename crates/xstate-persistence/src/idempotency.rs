//! The idempotency inbox: at-least-once delivery made safe by deduplicating at the consumer.
//!
//! Every broker, webhook provider and retrying HTTP client delivers *at least once*. The inbox
//! pattern keys each delivery by a stable id and answers a repeat from the stored receipt before
//! the machine sees it. This module ports the Python `persistence/idempotency.py`:
//!
//! * [`InboxStore`] with [`MemoryInbox`] and (feature `sqlite`) [`SqliteInbox`], using the same
//!   `inbox` table as Python so a database file can be shared between runtimes;
//! * [`scope`], [`validate_idempotency_key`], [`validate_principal`] and [`fingerprint`], all
//!   byte-compatible with Python (the fingerprint is the SHA-256 of
//!   `json.dumps({"type", "payload"}, sort_keys=True, separators=(",", ":"))`);
//! * [`IdempotencyGuard`], the decision logic of the Python `IdempotencyPlugin.on_before_send`,
//!   returning an [`Admission`];
//! * the in-snapshot "processed ring" (`__xsm_processed_ids__` / `__xsm_processed_at__`) that
//!   repairs a lost or half-written inbox entry after a crash.
//!
//! # Crash consistency
//!
//! The Python plugin can buffer the inbox mark and write it inside the store's lock transaction
//! when both share one SQLite connection. **That shared-transaction mode is not ported**: the
//! Rust inboxes own their connection. Use the save-then-mark order plus the ring
//! ([`record_processed`], [`IdempotencyGuard::check_with_ring`]), which is Python's
//! "mechanism 2" and covers the crash window between save and mark. Side effects performed by
//! actions are not covered by either mechanism: put them in services or an outbox.
//!
//! # Not ported
//!
//! The plugin plumbing (`on_before_send` / `on_event_processed` hooks, the per-session mark
//! buffer, `flush_marks` / `discard_marks`, the `_pending` claim table keyed by event identity)
//! belongs to the interpreter integration. The caller holds the [`Claim`] instead and settles it
//! with [`Claim::mark`] / [`Claim::release`] / [`Claim::settle`].

use std::fmt;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use xstate_core::Event;

use crate::receipts::pyjson;
use crate::store::{Result, StoreError};

mod guard;
mod memory;
mod ring;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use guard::{Admission, Claim, IdempotencyGuard, InboxErrorPolicy, RingView};
pub use memory::MemoryInbox;
pub use ring::{processed_ids, record_processed, ring_contains, ring_evidence};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteInbox;
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::sqlite_util;

/// Default retention of a processed key: 7 days, Stripe's own idempotency window.
pub const DEFAULT_TTL_S: f64 = 7.0 * 86_400.0;
/// Longest accepted idempotency key (printable ASCII characters).
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
/// How many processed `scope|key` ids each instance remembers inside its snapshot.
pub const PROCESSED_RING_SIZE: usize = 64;
/// Context key holding the ring of processed `scope|key` ids.
pub const PROCESSED_IDS_KEY: &str = "__xsm_processed_ids__";
/// Context key holding `{ring id: [wall time processed, fingerprint]}` for the ids in the ring.
pub const PROCESSED_AT_KEY: &str = "__xsm_processed_at__";
/// Payload fields excluded from the fingerprint and read by [`default_key`].
pub const DEFAULT_KEY_FIELDS: [&str; 2] = ["idempotency_key", "id"];

/// Principals that are renderings of nothing, never an identity.
const NOT_A_PRINCIPAL: [&str; 3] = ["None", "null", "anonymous"];

// ------------------------------------------------------------------------------ validation

/// Validate an idempotency key: non-empty printable ASCII, at most
/// [`MAX_IDEMPOTENCY_KEY_LEN`] characters. Keys come from outside the trust boundary.
///
/// # Errors
/// [`StoreError::InvalidKey`] describing the violation.
pub fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(StoreError::InvalidKey("idempotency key must be a non-empty str".into()));
    }
    if key.chars().count() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(StoreError::InvalidKey(format!(
            "idempotency key exceeds {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }
    if !key.chars().all(|c| (' '..='\u{7e}').contains(&c)) {
        return Err(StoreError::InvalidKey("idempotency key must be printable ASCII".into()));
    }
    Ok(())
}

/// Validate a principal: a non-empty string that is not a placeholder (`"None"`, `"null"`,
/// `"anonymous"`). A shared placeholder scope would let unauthenticated callers replay each
/// other's receipts, so every adapter routes its principal through here.
///
/// # Errors
/// [`StoreError::InvalidKey`] when the value cannot identify one caller.
pub fn validate_principal(principal: &str) -> Result<()> {
    if principal.is_empty() {
        return Err(StoreError::InvalidKey(
            "principal must be a non-empty str identifying the authenticated caller".into(),
        ));
    }
    if NOT_A_PRINCIPAL.contains(&principal) {
        return Err(StoreError::InvalidKey(format!(
            "principal '{principal}' is a placeholder, not an identity; refuse unauthenticated \
             callers before they reach the inbox"
        )));
    }
    Ok(())
}

fn scope_part(part: &str) -> String {
    part.replace('%', "%25").replace('/', "%2F")
}

/// The inbox scope `principal/machine_id/instance_key`, each part escaped (`%` to `%25`,
/// `/` to `%2F`) so the join is injective: tenants and instances can never collide.
#[must_use]
pub fn scope(principal: &str, machine_id: &str, instance_key: &str) -> String {
    [principal, machine_id, instance_key].map(scope_part).join("/")
}

/// The idempotency key of an event: `payload["idempotency_key"]`, else `payload["id"]`.
/// `None` means "not deduplicated". Non-string values are rendered like Python's `str()`.
#[must_use]
pub fn default_key(event: &Event) -> Option<String> {
    DEFAULT_KEY_FIELDS.iter().find_map(|name| match event.payload.get(name)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("True".into()),
        Value::Bool(false) => Some("False".into()),
        other => Some(pyjson::dumps(other, false, false)),
    })
}

/// SHA-256 (hex) of the canonical JSON `{"payload": <payload minus key fields>, "type": <type>}`
/// (sorted keys, `(",", ":")` separators, ASCII-escaped), so the same key with a different
/// payload is detectable. A `null` payload counts as `{}`.
#[must_use]
pub fn fingerprint<S: AsRef<str>>(event_type: &str, payload: &Value, key_fields: &[S]) -> String {
    let payload = match payload {
        Value::Null => Value::Object(Map::new()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !key_fields.iter().any(|f| f.as_ref() == k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    };
    let mut doc = Map::new();
    doc.insert("type".into(), Value::String(event_type.to_owned()));
    doc.insert("payload".into(), payload);
    let canon = pyjson::dumps(&Value::Object(doc), true, true);
    hex::encode(Sha256::digest(canon.as_bytes()))
}

// ------------------------------------------------------------------------------- the store

/// What an inbox remembers per `(scope, key)`.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
    /// [`fingerprint`] of the first delivery.
    pub fingerprint: String,
    /// The final receipt JSON; `None` while the first delivery is in flight.
    pub receipt_json: Option<String>,
    /// Expiry, epoch seconds; `None` = never.
    pub expires_at: Option<f64>,
}

/// Where processed idempotency keys live. Implementations are safe to share between threads
/// and, for durable backends, processes; [`claim`](Self::claim) is the primitive that makes
/// "first delivery wins" atomic.
pub trait InboxStore: Send + Sync + fmt::Debug {
    /// The entry, or `None` when unseen or expired.
    ///
    /// # Errors
    /// Backend failure.
    fn get(&self, scope: &str, key: &str) -> Result<Option<InboxEntry>>;

    /// Atomically record `(scope, key)` as in flight with `fingerprint`. `true` when this
    /// caller won (the key was unseen or expired), `false` when a live entry exists.
    ///
    /// # Errors
    /// Backend failure.
    fn claim(&self, scope: &str, key: &str, fingerprint: &str, ttl_s: Option<f64>) -> Result<bool>;

    /// Store the final receipt for a claimed key.
    ///
    /// # Errors
    /// Backend failure.
    fn mark(&self, scope: &str, key: &str, receipt_json: &str, ttl_s: Option<f64>) -> Result<()>;

    /// Drop an in-flight claim (the delivery failed before a receipt existed) so a retry can
    /// claim it again. A marked entry is left alone.
    ///
    /// # Errors
    /// Backend failure.
    fn release(&self, scope: &str, key: &str) -> Result<()>;

    /// Delete entries expired at `now` (default: the system clock); returns how many.
    ///
    /// # Errors
    /// Backend failure.
    fn purge_expired(&self, now: Option<f64>) -> Result<u64>;

    /// Delete every entry of `scope` (right to erasure); returns how many.
    ///
    /// # Errors
    /// Backend failure.
    fn forget(&self, scope: &str) -> Result<u64>;
}

pub(super) fn expiry(ttl_s: Option<f64>, now: f64) -> Option<f64> {
    ttl_s.map(|ttl| now + ttl)
}

pub(super) fn is_live(expires_at: Option<f64>, now: f64) -> bool {
    expires_at.is_none_or(|at| at > now)
}

pub(super) fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
