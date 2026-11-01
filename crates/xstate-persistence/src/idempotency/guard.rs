//! The guard: Python `IdempotencyPlugin.on_before_send` without the plugin plumbing.

use std::sync::Arc;

use serde_json::Value;
use xstate_core::Event;

use super::{
    DEFAULT_KEY_FIELDS, DEFAULT_TTL_S, InboxEntry, InboxStore, default_key, fingerprint,
    ring_contains, ring_evidence, scope, validate_idempotency_key, validate_principal,
};
use crate::receipts::{ReceiptError, ReceiptErrorKind, ReceiptJson};
use crate::store::{Result, StoreError};

/// What the guard does when the inbox backend itself fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboxErrorPolicy {
    /// Refuse the event: [`IdempotencyGuard::check`] returns `Err(StoreError::Unavailable)`,
    /// which adapters map to HTTP 503. The action never ran. The whole point of the guard is to
    /// not run twice, so this is the default.
    #[default]
    Refuse,
    /// Availability over dedup: admit the event unprotected and log the failure. For a webhook
    /// endpoint whose sender retries anyway and tolerates a duplicate.
    Admit,
}

/// The snapshot-side evidence [`IdempotencyGuard::check_with_ring`] consults to repair a lost or
/// half-written inbox entry.
#[derive(Debug, Clone, Copy)]
pub struct RingView<'a> {
    /// The instance context holding the ring.
    pub context: &'a Value,
    /// Current active leaf ids, for the conservative duplicate receipt.
    pub state_ids: &'a [String],
    /// Wall clock for the TTL comparison (epoch seconds).
    pub now_wall: f64,
}

/// The decision on one delivery.
#[derive(Debug)]
#[must_use]
pub enum Admission {
    /// First delivery: process the event, then settle the [`Claim`].
    Admit(Claim),
    /// A repeat: answer with this receipt (`duplicate` is set) instead of processing.
    Duplicate(ReceiptJson),
    /// The first delivery with this key is still being processed (HTTP 409).
    InFlight,
    /// The key was used for a different event (HTTP 422).
    Mismatch,
}

impl Admission {
    /// The refusal receipt Python's `_refuse` builds for [`InFlight`](Self::InFlight) and
    /// [`Mismatch`](Self::Mismatch): nothing ran, `duplicate` is set, `error` says why.
    /// `None` for the other variants.
    #[must_use]
    pub fn refusal_receipt(&self, key: &str, state_ids: &[String]) -> Option<ReceiptJson> {
        let (kind, message) = match self {
            Self::InFlight => (
                ReceiptErrorKind::IdempotencyInFlight,
                format!("Idempotency key {} is being processed; retry shortly.", py_repr(key)),
            ),
            Self::Mismatch => (
                ReceiptErrorKind::IdempotencyMismatch,
                format!(
                    "Idempotency key {} was already used for a different event; keys must not \
                     be reused with a different payload.",
                    py_repr(key)
                ),
            ),
            Self::Admit(_) | Self::Duplicate(_) => return None,
        };
        Some(ReceiptJson {
            state_ids: state_ids.to_vec(),
            error: Some(ReceiptError::new(kind, message)),
            duplicate: true,
            ..ReceiptJson::default()
        })
    }
}

/// Python `repr()` of a string, for messages that must read like the Python library's.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let body = s.replace('\\', "\\\\").replace(quote, &format!("\\{quote}"));
    format!("{quote}{body}{quote}")
}

#[derive(Debug)]
struct ClaimInner {
    inbox: Arc<dyn InboxStore>,
    scope: String,
    key: String,
    fingerprint: String,
    ttl_s: Option<f64>,
}

/// The right to process one keyed delivery, held between [`Admission::Admit`] and the end of
/// processing. Settle it with [`mark`](Self::mark) (the receipt exists), [`release`](Self::release)
/// (the delivery failed before taking effect) or [`settle`](Self::settle) (Python's rule).
///
/// Dropping an unsettled claim leaves the key *in flight* until its TTL, like a crashed worker:
/// conservative (HTTP 409), never a double run.
#[derive(Debug)]
pub struct Claim {
    inner: Option<ClaimInner>,
}

impl Claim {
    /// A claim that tracks nothing (event without a key, or `InboxErrorPolicy::Admit` after an
    /// inbox failure); all settle methods are no-ops.
    fn untracked() -> Self {
        Self { inner: None }
    }

    /// `true` when the claim is backed by an inbox row.
    #[must_use]
    pub fn is_tracked(&self) -> bool {
        self.inner.is_some()
    }

    /// The inbox scope, when tracked.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        self.inner.as_ref().map(|c| c.scope.as_str())
    }

    /// The idempotency key, when tracked.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.inner.as_ref().map(|c| c.key.as_str())
    }

    /// The fingerprint of the claimed event, when tracked.
    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        self.inner.as_ref().map(|c| c.fingerprint.as_str())
    }

    /// Store the final receipt. If the in-flight row vanished meanwhile (a TTL purge while the
    /// event ran), it is re-claimed with the real fingerprint first so the mark sticks.
    ///
    /// # Errors
    /// Backend failure.
    pub fn mark(self, receipt: &ReceiptJson) -> Result<()> {
        let Some(c) = self.inner else { return Ok(()) };
        if c.inbox.get(&c.scope, &c.key)?.is_none() {
            c.inbox.claim(&c.scope, &c.key, &c.fingerprint, c.ttl_s)?;
        }
        c.inbox.mark(&c.scope, &c.key, &receipt.to_json(), c.ttl_s)
    }

    /// Drop the in-flight claim so a retry is a first delivery.
    ///
    /// # Errors
    /// Backend failure.
    pub fn release(self) -> Result<()> {
        match self.inner {
            Some(c) => c.inbox.release(&c.scope, &c.key),
            None => Ok(()),
        }
    }

    /// Python's settle rule: an errored receipt that changed nothing did not take effect, so the
    /// claim is released and a retry may try again; every other receipt (a guard denial
    /// included, which *is* that payload's answer in this state) is marked.
    ///
    /// # Errors
    /// Backend failure.
    pub fn settle(self, receipt: &ReceiptJson) -> Result<()> {
        if receipt.error.is_some() && !receipt.changed {
            self.release()
        } else {
            self.mark(receipt)
        }
    }
}

/// The decision logic of the Python `IdempotencyPlugin`, without the plugin plumbing.
#[derive(Debug, Clone)]
pub struct IdempotencyGuard {
    /// Where keys live.
    pub inbox: Arc<dyn InboxStore>,
    /// The authenticated caller (validated by [`validate_principal`]).
    pub principal: String,
    /// Retention of a processed key; `None` = forever. Must be finite and `>= 0`.
    pub ttl_s: Option<f64>,
    /// Payload fields excluded from the fingerprint.
    pub key_fields: Vec<String>,
    /// What to do when the inbox backend fails.
    pub on_inbox_error: InboxErrorPolicy,
}

impl IdempotencyGuard {
    /// A guard with the Python defaults: 7-day TTL, `idempotency_key` / `id` key fields and
    /// [`InboxErrorPolicy::Refuse`].
    #[must_use]
    pub fn new(inbox: Arc<dyn InboxStore>, principal: impl Into<String>) -> Self {
        Self {
            inbox,
            principal: principal.into(),
            ttl_s: Some(DEFAULT_TTL_S),
            key_fields: DEFAULT_KEY_FIELDS.iter().map(|f| (*f).to_owned()).collect(),
            on_inbox_error: InboxErrorPolicy::Refuse,
        }
    }

    /// The inbox scope for an instance: `principal/machine_id/instance_key`, escaped.
    ///
    /// # Errors
    /// [`StoreError::InvalidKey`] when the principal fails [`validate_principal`].
    pub fn scope_for(&self, machine_id: &str, instance_key: &str) -> Result<String> {
        validate_principal(&self.principal)?;
        Ok(scope(&self.principal, machine_id, instance_key))
    }

    /// Decide what to do with a delivery.
    ///
    /// An event without a key ([`default_key`]) is admitted with an untracked claim. Without
    /// ring evidence a repeat whose first delivery crashed between save and mark reads as
    /// [`Admission::InFlight`]; use [`check_with_ring`](Self::check_with_ring) to repair it.
    ///
    /// # Errors
    /// [`StoreError::InvalidKey`] for a key that is not printable ASCII ≤ 255 or a bad TTL;
    /// with [`InboxErrorPolicy::Refuse`], [`StoreError::Unavailable`] when the inbox fails
    /// (Python's `InboxUnavailableError`, HTTP 503).
    pub fn check(&self, scope: &str, event: &Event) -> Result<Admission> {
        self.check_inner(scope, event, None)
    }

    /// [`check`](Self::check) with the snapshot's processed ring, implementing Python's
    /// "mechanism 2": a key the inbox shows in flight but the snapshot shows processed is a
    /// crash between save and mark (answered as a conservative duplicate, inbox repaired); a
    /// key the inbox lost but the ring saw within the TTL is a duplicate (or a mismatch when
    /// the fingerprints differ); older evidence is a genuine expiry and is re-admitted.
    ///
    /// # Errors
    /// As [`check`](Self::check).
    pub fn check_with_ring(
        &self,
        scope: &str,
        event: &Event,
        ring: &RingView<'_>,
    ) -> Result<Admission> {
        self.check_inner(scope, event, Some(ring))
    }

    fn check_inner(
        &self,
        scope: &str,
        event: &Event,
        ring: Option<&RingView<'_>>,
    ) -> Result<Admission> {
        let Some(key) = default_key(event) else {
            return Ok(Admission::Admit(Claim::untracked()));
        };
        validate_idempotency_key(&key)?;
        if self.ttl_s.is_some_and(|ttl| !ttl.is_finite() || ttl < 0.0) {
            return Err(StoreError::InvalidKey(format!(
                "ttl_s must be None or a number >= 0, got {:?}",
                self.ttl_s
            )));
        }
        let fp = fingerprint(&event.ty, &event.payload, &self.key_fields);
        match self.admit(scope, &key, &fp, ring) {
            Ok(admission) => Ok(admission),
            Err(error) => match self.on_inbox_error {
                InboxErrorPolicy::Refuse => Err(StoreError::Unavailable(format!(
                    "Idempotency inbox unavailable while checking key {}: {}; retry later.",
                    py_repr(&key),
                    error_label(&error)
                ))),
                InboxErrorPolicy::Admit => {
                    tracing::error!(
                        target: "xstate",
                        key = %key,
                        error = %error,
                        "idempotency inbox unavailable; admitting the event without deduplication"
                    );
                    Ok(Admission::Admit(Claim::untracked()))
                }
            },
        }
    }

    fn admit(
        &self,
        scope: &str,
        key: &str,
        fp: &str,
        ring: Option<&RingView<'_>>,
    ) -> Result<Admission> {
        let ring_id = format!("{scope}|{key}");
        let states = ring.map_or(&[][..], |r| r.state_ids);
        if let Some(entry) = self.inbox.get(scope, key)? {
            if entry.fingerprint != fp {
                return Ok(Admission::Mismatch);
            }
            if let Some(json) = &entry.receipt_json {
                return Ok(Admission::Duplicate(cached_receipt(json, states)));
            }
            // In flight per the inbox but processed per the snapshot: the crash window between
            // save and mark. The snapshot is the truth; answer a duplicate and repair.
            if ring.is_some_and(|r| ring_contains(r.context, &ring_id)) {
                let receipt = conservative_duplicate(states);
                let _ = self.inbox.mark(scope, key, &receipt.to_json(), self.ttl_s);
                return Ok(Admission::Duplicate(receipt));
            }
            return Ok(Admission::InFlight);
        }
        if let Some(r) = ring
            && let Some(evidence) = ring_evidence(r.context, &ring_id, self.ttl_s, r.now_wall)
        {
            if evidence != fp {
                return Ok(Admission::Mismatch);
            }
            let receipt = conservative_duplicate(states);
            if self.inbox.claim(scope, key, fp, self.ttl_s).unwrap_or(false) {
                let _ = self.inbox.mark(scope, key, &receipt.to_json(), self.ttl_s);
            }
            return Ok(Admission::Duplicate(receipt));
        }
        if self.inbox.claim(scope, key, fp, self.ttl_s)? {
            return Ok(Admission::Admit(Claim {
                inner: Some(ClaimInner {
                    inbox: Arc::clone(&self.inbox),
                    scope: scope.to_owned(),
                    key: key.to_owned(),
                    fingerprint: fp.to_owned(),
                    ttl_s: self.ttl_s,
                }),
            }));
        }
        // Lost the race to another worker between get and claim.
        Ok(match self.inbox.get(scope, key)? {
            Some(e) if e.fingerprint != fp => Admission::Mismatch,
            Some(InboxEntry { receipt_json: Some(json), .. }) => {
                Admission::Duplicate(cached_receipt(&json, states))
            }
            _ => Admission::InFlight,
        })
    }
}

/// A receipt that proves the key was processed without knowing the original outcome.
fn conservative_duplicate(state_ids: &[String]) -> ReceiptJson {
    ReceiptJson { state_ids: state_ids.to_vec(), duplicate: true, ..ReceiptJson::default() }
}

/// The stored receipt with `duplicate` set. A record that does not decode (hand-edited,
/// truncated) still proves the key was processed: answer a conservative duplicate, never
/// re-admit.
fn cached_receipt(json: &str, state_ids: &[String]) -> ReceiptJson {
    ReceiptJson::from_json(json).map_or_else(
        |error| {
            tracing::warn!(
                target: "xstate",
                %error,
                "cached idempotency receipt is malformed; answering a conservative duplicate"
            );
            conservative_duplicate(state_ids)
        },
        ReceiptJson::into_duplicate,
    )
}

/// Python class name of a failure, so the refusal message does not leak backend details.
fn error_label(error: &StoreError) -> &'static str {
    match error {
        StoreError::Io(_) => "OSError",
        StoreError::Unavailable(_) => "StoreUnavailableError",
        _ => "StoreError",
    }
}
