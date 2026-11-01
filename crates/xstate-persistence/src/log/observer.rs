//! [`TransitionLogObserver`]: one [`TransitionRecord`] per processed event.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};
use xstate_core::config::UnhandledPolicy;
use xstate_core::snapshot::python_event_name;
use xstate_core::{ActiveState, Context, Error, Event, EventOrigin, Machine, Observer, StepReport};

use super::record::{Disposition, Origin, TransitionLogStore, TransitionRecord};
use crate::receipts::ReceiptErrorKind;
use crate::store::wall_now;

/// Payload keys masked by default (case-insensitive substring match, `-` read as `_`): the
/// Python `DEFAULT_REDACT_KEYS`.
pub const DEFAULT_REDACT_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "auth",
    "bearer",
    "credential",
    "private_key",
    "cookie",
    "session",
    "signature",
    "otp",
    "pin",
    "card",
    "cvv",
    "cvc",
    "pan",
    "iban",
    "account_number",
    "account_no",
    "routing",
    "swift",
    "mnemonic",
    "seed_phrase",
    "seed",
    "ssn",
    "dob",
    "date_of_birth",
    "email",
    "phone",
    "passport",
];

/// A shared `null`, borrowed where an event has no payload.
static NULL: Value = Value::Null;

/// Replacement for a redacted value.
const MASK: &str = "***";

fn normalise(key: &str) -> String {
    key.to_lowercase().replace('-', "_")
}

/// A copy of `value` with every object member whose key contains one of `keys` (case-insensitive
/// substring, `-` treated as `_` on both sides) replaced by `"***"`. Recurses into objects and
/// arrays; never mutates its input.
#[must_use]
pub fn redact<S: AsRef<str>>(value: &Value, keys: &[S]) -> Value {
    let normalised: Vec<String> = keys.iter().map(|k| normalise(k.as_ref())).collect();
    redact_with(value, &normalised)
}

fn redact_with(value: &Value, keys: &[String]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let key = normalise(k);
                    let masked = keys.iter().any(|needle| key.contains(needle.as_str()));
                    let v = if masked { Value::String(MASK.into()) } else { redact_with(v, keys) };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact_with(v, keys)).collect()),
        other => other.clone(),
    }
}

/// Payload keys the observer reads the audit fields from (the Python `AuditPlugin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditKeys {
    /// Key of the actor (who).
    pub actor: String,
    /// Key of the reason (why).
    pub reason: String,
    /// Key of the correlation id.
    pub correlation: String,
}

impl Default for AuditKeys {
    fn default() -> Self {
        Self {
            actor: "actor".into(),
            reason: "reason".into(),
            correlation: "correlation_id".into(),
        }
    }
}

/// Everything the record needs beyond the state sets and the clock.
struct Outcome<'a> {
    event_type: String,
    payload: Map<String, Value>,
    actions: Vec<String>,
    disposition: Disposition,
    engine: bool,
    origin: Origin,
    error: Option<Value>,
    raw_payload: &'a Value,
}

/// Appends one [`TransitionRecord`] per processed event to a [`TransitionLogStore`].
///
/// Attach it with `interpreter.observe(Arc::new(observer))`. The leaf ids *before* each step
/// are kept in a mutex inside the observer (initialised in `on_start`, advanced after every
/// event), because [`Observer::on_event`] only sees the state after the step.
///
/// What is recorded, and how it differs from the Python `TransitionLogPlugin`:
///
/// * a processed event gets `transition` (it changed something), `unhandled`, `deferred`
///   (unhandled under `onUnhandled: "defer"`) or `error` (an action failed and the step was
///   rolled back);
/// * an event rejected because the machine is not running is an `error` record
///   (`InterpreterStoppedError`);
/// * a step that fails with an error is an `error` record with an empty `event_type`, because
///   [`Observer::on_error`] does not receive the event;
/// * `denied` and `duplicate` are never written (the engine cannot tell a guard refusal from a
///   missing handler; duplicates are answered before the machine);
/// * an event deferred earlier and re-released later is recorded as `external` (the engine
///   re-queues the original event); Python records it as `internal`.
///
/// `engine` is the Python flag: `true` for timer and service-completion events. Records are
/// appended through [`TransitionLogStore::append_next`]; a failing store is reported through
/// `tracing` and never stops the machine (observers are infallible by design).
///
/// TODO(replay): the Python `replay()` (re-run recorded external events from the initial
/// context and assert every step lands where it was recorded) is not ported yet.
pub struct TransitionLogObserver {
    log: Arc<dyn TransitionLogStore>,
    include_non_transitions: bool,
    redact_keys: Vec<String>,
    machine_id: Option<String>,
    audit: Option<AuditKeys>,
    last_states: Mutex<Vec<String>>,
}

impl std::fmt::Debug for TransitionLogObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransitionLogObserver")
            .field("log", &self.log)
            .field("include_non_transitions", &self.include_non_transitions)
            .field("machine_id", &self.machine_id)
            .finish_non_exhaustive()
    }
}

impl TransitionLogObserver {
    /// An observer writing to `log`, recording non-transitions too, redacting
    /// [`DEFAULT_REDACT_KEYS`], keyed by the machine id.
    #[must_use]
    pub fn new(log: Arc<dyn TransitionLogStore>) -> Self {
        Self {
            log,
            include_non_transitions: true,
            redact_keys: DEFAULT_REDACT_KEYS.iter().map(|k| (*k).to_owned()).collect(),
            machine_id: None,
            audit: None,
            last_states: Mutex::new(Vec::new()),
        }
    }

    /// Record only events that produced a transition (default: also record unhandled,
    /// deferred and errored ones; an audit that omits refused attempts is not an audit).
    #[must_use]
    pub fn include_non_transitions(mut self, include: bool) -> Self {
        self.include_non_transitions = include;
        self
    }

    /// Replace the redaction denylist; an empty list logs payloads verbatim.
    #[must_use]
    pub fn redact_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.redact_keys = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Use this log key (the store key of a persisted instance) instead of the machine id.
    #[must_use]
    pub fn machine_id(mut self, key: impl Into<String>) -> Self {
        self.machine_id = Some(key.into());
        self
    }

    /// Also fill `actor` / `reason` / `correlation_id` from the event payload (the Python
    /// `AuditPlugin`).
    #[must_use]
    pub fn audit(mut self, keys: AuditKeys) -> Self {
        self.audit = Some(keys);
        self
    }

    fn states(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.last_states.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn audit_field(&self, payload: &Value, pick: impl Fn(&AuditKeys) -> &str) -> Option<String> {
        let keys = self.audit.as_ref()?;
        match payload.get(pick(keys))? {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }

    fn write(&self, machine: &Machine, from: Vec<String>, to: Vec<String>, outcome: Outcome<'_>) {
        if outcome.disposition != Disposition::Transition && !self.include_non_transitions {
            return;
        }
        let record = TransitionRecord {
            machine_id: self.machine_id.clone().unwrap_or_else(|| machine.id().to_owned()),
            seq: 0, // assigned by the store at write time (gap-free)
            ts: wall_now(),
            event_type: outcome.event_type,
            event_payload: outcome.payload,
            from_states: from,
            to_states: to,
            actions: outcome.actions,
            disposition: outcome.disposition,
            actor: self.audit_field(outcome.raw_payload, |k| &k.actor),
            reason: self.audit_field(outcome.raw_payload, |k| &k.reason),
            correlation_id: self.audit_field(outcome.raw_payload, |k| &k.correlation),
            machine_version: machine.version().unwrap_or_default().to_owned(),
            engine: outcome.engine,
            error: outcome.error,
            origin: outcome.origin,
        };
        if let Err(error) = self.log.append_next(record) {
            tracing::error!(target: "xstate", machine = machine.id(), %error, "transition log append failed");
        }
    }

    fn payload_of(&self, event: &Event) -> Map<String, Value> {
        let payload = match event.origin {
            EventOrigin::After => json_object([("kind", Value::from("after"))]),
            EventOrigin::DoneActor => json_object([
                ("kind", Value::from("done")),
                ("src", Value::from(suffix(&event.ty, "xstate.done.actor."))),
                ("data", event.payload.get("output").cloned().unwrap_or(Value::Null)),
            ]),
            EventOrigin::ErrorActor => {
                let message = match event.payload.get("error") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                json_object([
                    ("kind", Value::from("error")),
                    ("src", Value::from(suffix(&event.ty, "xstate.error.actor."))),
                    (
                        "error",
                        Value::Object(json_object([
                            ("type", Value::from("Error")),
                            ("message", Value::from(message)),
                        ])),
                    ),
                ])
            }
            _ => match &event.payload {
                Value::Null => Map::new(),
                Value::Object(map) => map.clone(),
                other => json_object([("value", other.clone())]),
            },
        };
        match redact_with(
            &Value::Object(payload),
            &self.redact_keys.iter().map(|k| normalise(k)).collect::<Vec<_>>(),
        ) {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }
}

fn json_object<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

fn suffix<'a>(ty: &'a str, prefix: &str) -> &'a str {
    ty.strip_prefix(prefix).unwrap_or(ty)
}

fn error_json(kind: &ReceiptErrorKind, message: &str) -> Value {
    Value::Object(json_object([
        ("type", Value::from(kind.type_name())),
        ("message", Value::from(message)),
    ]))
}

impl<C: Context> Observer<C> for TransitionLogObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, _report: &StepReport) {
        *self.states() = state.leaf_ids(machine);
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let to = state.leaf_ids(machine);
        let from = std::mem::replace(&mut *self.states(), to.clone());
        let defers = machine.policies().on_unhandled == UnhandledPolicy::Defer
            && event.origin == EventOrigin::External;
        let (disposition, error) = if let Some(failed) = &report.failed {
            (
                Disposition::Error,
                Some(error_json(
                    &ReceiptErrorKind::from_engine(xstate_core::ErrorKind::TransitionFailed),
                    failed,
                )),
            )
        } else if report.unhandled && defers {
            (Disposition::Deferred, None)
        } else if report.changed {
            (Disposition::Transition, None)
        } else {
            (Disposition::Unhandled, None)
        };
        let engine = matches!(
            event.origin,
            EventOrigin::After | EventOrigin::DoneActor | EventOrigin::ErrorActor
        );
        let outcome = Outcome {
            event_type: python_event_name(&event.ty),
            payload: self.payload_of(event),
            actions: report.actions.iter().map(ToString::to_string).collect(),
            disposition,
            engine,
            origin: if event.origin == EventOrigin::External {
                Origin::External
            } else {
                Origin::Internal
            },
            error,
            raw_payload: &event.payload,
        };
        self.write(machine, from, to, outcome);
    }

    fn on_event_dropped(&self, machine: &Machine, event: &Event, reason: &'static str) {
        // "deferred" was already recorded by `on_event` (the event was processed as unhandled
        // first); an evicted deferred event was recorded when it was parked.
        if reason != "not_running" {
            return;
        }
        let states = self.states().clone();
        let message = format!("interpreter is not running; event '{}' was not processed", event.ty);
        let outcome = Outcome {
            event_type: python_event_name(&event.ty),
            payload: self.payload_of(event),
            actions: Vec::new(),
            disposition: Disposition::Error,
            engine: false,
            origin: if event.origin == EventOrigin::External {
                Origin::External
            } else {
                Origin::Internal
            },
            error: Some(error_json(&ReceiptErrorKind::InterpreterStopped, &message)),
            raw_payload: &event.payload,
        };
        self.write(machine, states.clone(), states, outcome);
    }

    fn on_error(&self, machine: &Machine, error: &Error) {
        let states = self.states().clone();
        let kind = ReceiptErrorKind::from_engine(error.kind());
        let outcome = Outcome {
            event_type: String::new(),
            payload: Map::new(),
            actions: Vec::new(),
            disposition: Disposition::Error,
            engine: false,
            origin: Origin::External,
            error: Some(error_json(&kind, &error.to_string())),
            raw_payload: &NULL,
        };
        self.write(machine, states.clone(), states, outcome);
    }
}
