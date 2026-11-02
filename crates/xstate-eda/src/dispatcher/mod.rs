//! [`InboundDispatcher`]: envelope → machine instance → event (Python `eda/dispatcher.py`).
//!
//! The generic consumer loop every broker integration reuses:
//!
//! ```text
//! delivery → validate → machine_for_type(type) → [inbox dedup] → handler(envelope, event,
//!            machine, key = subject) → ack
//! ```
//!
//! * **Runtime-agnostic.** Where Python runs `persisted()` + `interp.send` itself, the caller
//!   supplies an [`EnvelopeHandler`] (any `Fn(&Dispatch) -> Result<ReceiptJson, HandlerError>`)
//!   that loads the instance (typically with `xstate_persistence::persisted`), sends
//!   [`Dispatch::event`] and returns the receipt.
//! * **Dedup.** With an inbox, the envelope id (or [`dedup_key`](InboundDispatcher::dedup_key))
//!   is claimed in scope `source/machine id/key` before the handler runs and marked with the
//!   receipt after it succeeded; a redelivery is answered as `duplicate`.
//! * **Order.** One subject is processed at a time; different subjects run concurrently up to
//!   `max_in_flight` in [`run_once`](InboundDispatcher::run_once). When a delivery fails and is
//!   requeued, the later deliveries of the same subject held in this batch are requeued behind
//!   it.
//! * **Poison (X0.8).** Attempts = max(envelope `xsmattempt`, the dispatcher's own count). At
//!   `max_attempts` the envelope is dead-lettered and acked: never an infinite redelivery loop.
//!   An unknown `type` is dead-lettered as `unknown_event` and a malformed envelope as `corrupt`
//!   on the first attempt; neither is raised into the consumer loop.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use xstate_core::{Event, Machine};
use xstate_persistence::receipts::ReceiptErrorKind;
use xstate_persistence::{InboxStore, ReceiptJson, StoreError};

use crate::dead_letter::{DeadLetterSink, MemoryDeadLetterStore};
use crate::envelope::Envelope;
use crate::error::{EdaError, Result};

mod consume;
mod handle;

/// Deliveries before a failing envelope is dead-lettered.
pub const DEFAULT_MAX_ATTEMPTS: u64 = 5;
/// Subjects processed concurrently by [`InboundDispatcher::run_once`].
pub const DEFAULT_MAX_IN_FLIGHT: usize = 16;
/// Upper bound on deliveries pulled into one batch.
pub const DEFAULT_MAX_BATCH: usize = 1000;
/// Envelope ids whose failure count the dispatcher remembers (least recently used dropped).
const ATTEMPTS_MEMORY: usize = 10_000;

/// What one `run_once` / `handle` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchResult {
    /// Envelopes the handler processed.
    pub processed: usize,
    /// Redeliveries answered from the inbox.
    pub duplicates: usize,
    /// Envelopes written to the dead-letter store (and acked).
    pub dead_lettered: usize,
    /// Envelopes nacked for redelivery.
    pub retried: usize,
    /// Unknown types acked under [`OnUnknown::Ignore`].
    pub ignored: usize,
    /// `(envelope id, outcome)`: `processed`, `duplicate`, `ignored`, `retry` or
    /// `dead_lettered:<reason>`.
    pub outcomes: Vec<(String, String)>,
}

impl DispatchResult {
    /// Add `other`'s counters and outcomes into this result.
    pub fn merge(&mut self, other: Self) {
        self.processed += other.processed;
        self.duplicates += other.duplicates;
        self.dead_lettered += other.dead_lettered;
        self.retried += other.retried;
        self.ignored += other.ignored;
        self.outcomes.extend(other.outcomes);
    }
}

/// What to do with an envelope type no machine handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnUnknown {
    /// Dead-letter it as `unknown_event` (default, X0.8).
    #[default]
    DeadLetter,
    /// Ack it: a choreography bus carries other services' events, which are not poison.
    Ignore,
}

/// Everything the handler needs for one envelope.
#[derive(Debug)]
pub struct Dispatch<'a> {
    /// The inbound envelope (set it as the outbox cause).
    pub envelope: &'a Envelope,
    /// The event to send (`default_event_name(type)` with `data` as payload).
    pub event: Event,
    /// The machine that handles the type.
    pub machine: &'a Machine,
    /// The instance store key (default the subject).
    pub key: &'a str,
    /// The topic the envelope came from.
    pub topic: Option<&'a str>,
}

/// Why a handler did not process an envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HandlerError {
    /// A transient failure: retried, then dead-lettered as `max_attempts` (Python
    /// `ProcessingFailedError` and every other exception raised while processing).
    #[error("{message}")]
    Failed {
        /// Python-spelled error class name for the dead-letter record.
        error_type: String,
        /// Error message.
        message: String,
    },
    /// The instance already finished: redelivering cannot help, so it is dead-lettered as
    /// `instance_done` at once.
    #[error("{message}")]
    InstanceDone {
        /// Error message.
        message: String,
    },
}

impl HandlerError {
    /// A transient failure with a Python-style class name.
    pub fn failed(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failed { error_type: error_type.into(), message: message.into() }
    }

    /// Python's rule for a send receipt: an error that is not a duplicate means "not
    /// committed" (`ProcessingFailedError`, or [`InstanceDone`](Self::InstanceDone) for an
    /// `InterpreterStoppedError`); anything else is returned unchanged.
    ///
    /// # Errors
    /// The receipt carries a non-duplicate error.
    pub fn check_receipt(receipt: ReceiptJson) -> Result<ReceiptJson, Self> {
        match &receipt.error {
            Some(err) if !receipt.duplicate => Err(match err.kind {
                ReceiptErrorKind::InterpreterStopped => {
                    Self::InstanceDone { message: err.message.clone() }
                }
                _ => Self::failed("ProcessingFailedError", err.message.clone()),
            }),
            _ => Ok(receipt),
        }
    }

    /// The class name written to the dead-letter record.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::Failed { error_type, .. } => error_type,
            Self::InstanceDone { .. } => "InterpreterStoppedError",
        }
    }
}

impl From<StoreError> for HandlerError {
    fn from(e: StoreError) -> Self {
        let name = match &e {
            StoreError::Conflict { .. } => "ConcurrencyError",
            StoreError::Unavailable(_) => "StoreUnavailableError",
            StoreError::LockTimeout { .. } => "LockTimeoutError",
            StoreError::SnapshotTooLarge { .. } => "SnapshotTooLargeError",
            StoreError::Corrupt(_) => "SnapshotCorruptError",
            StoreError::Engine(engine) => {
                return Self::failed(
                    ReceiptErrorKind::from_engine(engine.kind()).type_name(),
                    engine.to_string(),
                );
            }
            _ => "StoreError",
        };
        Self::failed(name, e.to_string())
    }
}

impl From<xstate_core::Error> for HandlerError {
    fn from(e: xstate_core::Error) -> Self {
        Self::failed(ReceiptErrorKind::from_engine(e.kind()).type_name(), e.to_string())
    }
}

/// Processes one envelope for the dispatcher.
pub trait EnvelopeHandler: Send + Sync {
    /// Load the instance `dispatch.key` of `dispatch.machine`, send `dispatch.event` and
    /// persist; return the receipt (marked in the inbox) or why it failed.
    ///
    /// # Errors
    /// See [`HandlerError`].
    fn handle(&self, dispatch: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError>;
}

impl<F> EnvelopeHandler for F
where
    F: Fn(&Dispatch<'_>) -> Result<ReceiptJson, HandlerError> + Send + Sync,
{
    fn handle(&self, dispatch: &Dispatch<'_>) -> Result<ReceiptJson, HandlerError> {
        self(dispatch)
    }
}

/// A machine lookup from a map of envelope types (Python passes a dict).
pub fn machines_by_type<S>(
    map: HashMap<String, Machine, S>,
) -> impl Fn(&str) -> Option<Machine> + Send + Sync + 'static
where
    S: std::hash::BuildHasher + Send + Sync + 'static,
{
    move |ty| map.get(ty).cloned()
}

type Lookup = Box<dyn Fn(&str) -> Option<Machine> + Send + Sync>;
type KeyFor = Box<dyn Fn(&Envelope, &Machine) -> String + Send + Sync>;
type DedupKey = Box<dyn Fn(&Envelope) -> Option<String> + Send + Sync>;
type EventType = Box<dyn Fn(&Envelope) -> String + Send + Sync>;

/// Failure counts per envelope id, least recently used evicted first.
#[derive(Default)]
struct Attempts {
    counts: HashMap<String, (u64, u64)>,
    order: BTreeMap<u64, String>,
    tick: u64,
}

impl Attempts {
    fn get(&self, id: &str) -> u64 {
        self.counts.get(id).map_or(0, |(n, _)| *n)
    }

    fn bump(&mut self, id: &str, floor: u64) -> u64 {
        let n = floor.max(self.get(id)) + 1;
        self.tick += 1;
        if let Some((_, stamp)) = self.counts.insert(id.to_owned(), (n, self.tick)) {
            self.order.remove(&stamp);
        }
        self.order.insert(self.tick, id.to_owned());
        while self.counts.len() > ATTEMPTS_MEMORY {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.counts.remove(&oldest);
        }
        n
    }

    fn forget(&mut self, id: &str) {
        if let Some((_, stamp)) = self.counts.remove(id) {
            self.order.remove(&stamp);
        }
    }
}

/// Consume envelopes into machine instances through an [`EnvelopeHandler`].
pub struct InboundDispatcher<H> {
    handler: H,
    lookup: Lookup,
    inbox: Option<Arc<dyn InboxStore>>,
    inbox_ttl_s: Option<f64>,
    max_in_flight: usize,
    max_attempts: u64,
    max_batch: usize,
    dead_letters: Arc<dyn DeadLetterSink>,
    event_type: Option<EventType>,
    key_for: KeyFor,
    dedup_key: DedupKey,
    on_unknown: OnUnknown,
    attempts: Mutex<Attempts>,
}

impl<H> std::fmt::Debug for InboundDispatcher<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundDispatcher")
            .field("has_inbox", &self.inbox.is_some())
            .field("max_in_flight", &self.max_in_flight)
            .field("max_attempts", &self.max_attempts)
            .field("max_batch", &self.max_batch)
            .field("on_unknown", &self.on_unknown)
            .finish_non_exhaustive()
    }
}

impl<H: EnvelopeHandler> InboundDispatcher<H> {
    /// A dispatcher with Python's defaults: no inbox, 5 attempts, 16 subjects in flight, an
    /// in-memory dead-letter store, key = subject, dedup key = envelope id.
    pub fn new(
        machine_for_type: impl Fn(&str) -> Option<Machine> + Send + Sync + 'static,
        handler: H,
    ) -> Self {
        Self {
            handler,
            lookup: Box::new(machine_for_type),
            inbox: None,
            inbox_ttl_s: Some(xstate_persistence::DEFAULT_TTL_S),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_batch: DEFAULT_MAX_BATCH,
            dead_letters: Arc::new(MemoryDeadLetterStore::new()),
            event_type: None,
            key_for: Box::new(|env, _| env.subject.clone().unwrap_or_else(|| "None".into())),
            dedup_key: Box::new(|env| Some(env.id.clone())),
            on_unknown: OnUnknown::DeadLetter,
            attempts: Mutex::new(Attempts::default()),
        }
    }

    /// Deduplicate through `inbox` (dedup key = envelope id by default).
    #[must_use]
    pub fn inbox(mut self, inbox: Arc<dyn InboxStore>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Retention of inbox entries (default 7 days; `None` = forever).
    #[must_use]
    pub fn inbox_ttl_s(mut self, ttl_s: Option<f64>) -> Self {
        self.inbox_ttl_s = ttl_s;
        self
    }

    /// Subjects processed concurrently by [`run_once`](Self::run_once).
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for 0.
    pub fn max_in_flight(mut self, n: usize) -> Result<Self> {
        if n < 1 {
            return Err(EdaError::InvalidArgument("max_in_flight must be >= 1".into()));
        }
        self.max_in_flight = n;
        Ok(self)
    }

    /// Deliveries before a failing envelope is dead-lettered.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] for 0.
    pub fn max_attempts(mut self, n: u64) -> Result<Self> {
        if n < 1 {
            return Err(EdaError::InvalidArgument("max_attempts must be >= 1".into()));
        }
        self.max_attempts = n;
        Ok(self)
    }

    /// Deliveries pulled into one batch.
    #[must_use]
    pub fn max_batch(mut self, n: usize) -> Self {
        self.max_batch = n.max(1);
        self
    }

    /// Where dead letters go (default an in-memory store private to the dispatcher).
    #[must_use]
    pub fn dead_letters(mut self, sink: Arc<dyn DeadLetterSink>) -> Self {
        self.dead_letters = sink;
        self
    }

    /// Event-name override (default [`default_event_name`](crate::default_event_name)).
    #[must_use]
    pub fn event_type(mut self, f: impl Fn(&Envelope) -> String + Send + Sync + 'static) -> Self {
        self.event_type = Some(Box::new(f));
        self
    }

    /// Store key of the instance (default the subject). A choreography router prefixes the
    /// machine id so two machines can own the same business key.
    #[must_use]
    pub fn key_for(
        mut self,
        f: impl Fn(&Envelope, &Machine) -> String + Send + Sync + 'static,
    ) -> Self {
        self.key_for = Box::new(f);
        self
    }

    /// Inbox key (default the envelope id; `None` / empty = not deduplicated). Sagas dedup on
    /// `causationid`.
    #[must_use]
    pub fn dedup_key(
        mut self,
        f: impl Fn(&Envelope) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.dedup_key = Box::new(f);
        self
    }

    /// Policy for types no machine handles.
    #[must_use]
    pub fn on_unknown(mut self, policy: OnUnknown) -> Self {
        self.on_unknown = policy;
        self
    }

    /// The machine for an envelope type.
    #[must_use]
    pub fn machine_for(&self, envelope_type: &str) -> Option<Machine> {
        (self.lookup)(envelope_type)
    }

    /// The handler.
    pub fn handler(&self) -> &H {
        &self.handler
    }

    fn attempts(&self) -> MutexGuard<'_, Attempts> {
        self.attempts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The larger of the envelope's own attempt and the locally counted one.
    #[must_use]
    pub fn attempts_of(&self, envelope: &Envelope) -> u64 {
        envelope.attempt().max(self.attempts().get(&envelope.id))
    }

    fn bump(&self, envelope: &Envelope) -> u64 {
        self.attempts().bump(&envelope.id, envelope.attempt())
    }

    fn forget(&self, envelope: &Envelope) {
        self.attempts().forget(&envelope.id);
    }
}
