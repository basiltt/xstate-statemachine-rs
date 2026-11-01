//! [`TransitionRecord`] and the [`TransitionLogStore`] contract.

use std::fmt;

use serde_json::{Map, Value};

use crate::receipts::pyjson;
use crate::store::{Result, StoreError};

/// What happened to the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// A transition was taken (or the context changed).
    Transition,
    /// A guard refused the event. The Rust engine does not distinguish a guard refusal from "no
    /// handler", so [`TransitionLogObserver`](super::TransitionLogObserver) never writes it;
    /// records from the Python runtime may carry it.
    Denied,
    /// No transition matched, or the event changed nothing.
    Unhandled,
    /// Held back by `onUnhandled: "defer"`.
    Deferred,
    /// Processing failed.
    Error,
    /// Answered from the idempotency inbox.
    Duplicate,
}

impl Disposition {
    /// The lower-case word stored in the `disposition` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transition => "transition",
            Self::Denied => "denied",
            Self::Unhandled => "unhandled",
            Self::Deferred => "deferred",
            Self::Error => "error",
            Self::Duplicate => "duplicate",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "transition" => Self::Transition,
            "denied" => Self::Denied,
            "unhandled" => Self::Unhandled,
            "deferred" => Self::Deferred,
            "error" => Self::Error,
            "duplicate" => Self::Duplicate,
            _ => return None,
        })
    }
}

/// Where the event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// A caller's `send()`.
    External,
    /// Produced inside a step (a `raise`, a self `sendTo`, a timer, a service completion).
    Internal,
}

impl Origin {
    /// The word stored in the `origin` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Internal => "internal",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "external" => Some(Self::External),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

/// One processed event and what it did: the Python `TransitionRecord`, field for field.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRecord {
    /// The log key: the store key of the persisted instance, else the machine id.
    pub machine_id: String,
    /// Gap-free per-key sequence, from 1.
    pub seq: u64,
    /// Epoch seconds when the event settled.
    pub ts: f64,
    /// Event type in the Python / v4 spelling (`done.invoke.x`, `after.500.m.a`, ...).
    pub event_type: String,
    /// The event payload, redacted. For engine-minted events it holds `kind` plus `data`
    /// (done) or `error` (`{type, message}`).
    pub event_payload: Map<String, Value>,
    /// Leaf ids before the event (sorted).
    pub from_states: Vec<String>,
    /// Leaf ids after the event (sorted); equals `from_states` for a non-transition.
    pub to_states: Vec<String>,
    /// User action names run, in order.
    pub actions: Vec<String>,
    /// What happened to the event.
    pub disposition: Disposition,
    /// Who did it (audit).
    pub actor: Option<String>,
    /// Why (audit).
    pub reason: Option<String>,
    /// Correlation id (audit).
    pub correlation_id: Option<String>,
    /// The chart `version` label, or `""`.
    pub machine_version: String,
    /// `true` for an engine-minted event (`after`, service completion / failure). This is the
    /// Python meaning of the field: it is a flag, not an engine name.
    pub engine: bool,
    /// `{type, message}` of the failure, when there was one.
    pub error: Option<Value>,
    /// Where the event came from.
    pub origin: Origin,
}

impl TransitionRecord {
    /// The JSON object of this record (Python `to_dict`).
    #[must_use]
    pub fn to_value(&self) -> Value {
        let strings =
            |v: &[String]| Value::Array(v.iter().map(|s| Value::String(s.clone())).collect());
        let opt = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        let mut map = Map::new();
        map.insert("machine_id".into(), Value::String(self.machine_id.clone()));
        map.insert("seq".into(), Value::from(self.seq));
        map.insert("ts".into(), Value::from(self.ts));
        map.insert("event_type".into(), Value::String(self.event_type.clone()));
        map.insert("event_payload".into(), Value::Object(self.event_payload.clone()));
        map.insert("from_states".into(), strings(&self.from_states));
        map.insert("to_states".into(), strings(&self.to_states));
        map.insert("actions".into(), strings(&self.actions));
        map.insert("disposition".into(), Value::String(self.disposition.as_str().into()));
        map.insert("actor".into(), opt(&self.actor));
        map.insert("reason".into(), opt(&self.reason));
        map.insert("correlation_id".into(), opt(&self.correlation_id));
        map.insert("machine_version".into(), Value::String(self.machine_version.clone()));
        map.insert("engine".into(), Value::Bool(self.engine));
        map.insert("error".into(), self.error.clone().unwrap_or(Value::Null));
        map.insert("origin".into(), Value::String(self.origin.as_str().into()));
        Value::Object(map)
    }

    /// The JSON text, formatted exactly like Python's
    /// `json.dumps(rec.to_dict(), sort_keys=True, default=str)`.
    #[must_use]
    pub fn to_json(&self) -> String {
        pyjson::dumps(&self.to_value(), true, false)
    }

    /// Strict inverse of [`to_value`](Self::to_value): anything that is not a record is
    /// corruption, never skipped (a replay over a hole would "succeed" to a wrong state).
    ///
    /// Compared with Python, an unknown `disposition` / `origin` word is also corruption, and
    /// `from_states` / `to_states` / `actions` must be lists of strings.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] naming the offending field.
    pub fn from_value(value: &Value) -> Result<Self> {
        Self::parse(value)
            .map_err(|why| StoreError::Corrupt(format!("not a TransitionRecord ({why})")))
    }

    /// Decode one JSON document; see [`from_value`](Self::from_value).
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for malformed JSON or a malformed record.
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| StoreError::Corrupt(format!("not a TransitionRecord (not JSON: {e})")))?;
        Self::from_value(&value)
    }

    fn parse(value: &Value) -> std::result::Result<Self, String> {
        let obj = value.as_object().ok_or("record is not an object")?;
        let seq = obj
            .get("seq")
            .and_then(Value::as_u64)
            .filter(|s| *s >= 1)
            .ok_or("seq must be an integer >= 1")?;
        let ts = obj
            .get("ts")
            .and_then(Value::as_f64)
            .filter(|t| t.is_finite())
            .ok_or("ts must be a finite number")?;
        let payload = match obj.get("event_payload") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err("event_payload is not an object".into()),
        };
        let disposition = match obj.get("disposition") {
            None | Some(Value::Null) => Disposition::Transition,
            Some(Value::String(s)) => {
                Disposition::parse(s).ok_or_else(|| format!("unknown disposition '{s}'"))?
            }
            Some(_) => return Err("disposition is not a string".into()),
        };
        let origin = match obj.get("origin") {
            None | Some(Value::Null) => Origin::External,
            Some(Value::String(s)) => {
                Origin::parse(s).ok_or_else(|| format!("unknown origin '{s}'"))?
            }
            Some(_) => return Err("origin is not a string".into()),
        };
        let engine = match obj.get("engine") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("engine is not a boolean".into()),
        };
        Ok(Self {
            machine_id: required_str(obj, "machine_id")?,
            seq,
            ts,
            event_type: required_str(obj, "event_type")?,
            event_payload: payload,
            from_states: string_list(obj, "from_states")?,
            to_states: string_list(obj, "to_states")?,
            actions: string_list(obj, "actions")?,
            disposition,
            actor: optional_str(obj, "actor")?,
            reason: optional_str(obj, "reason")?,
            correlation_id: optional_str(obj, "correlation_id")?,
            machine_version: optional_str(obj, "machine_version")?.unwrap_or_default(),
            engine,
            error: obj.get("error").filter(|v| !v.is_null()).cloned(),
            origin,
        })
    }
}

fn required_str(obj: &Map<String, Value>, key: &str) -> std::result::Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} is missing or not a string"))
}

fn optional_str(
    obj: &Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key} is not a string")),
    }
}

fn string_list(obj: &Map<String, Value>, key: &str) -> std::result::Result<Vec<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("{key} is not a list of strings")),
        Some(_) => Err(format!("{key} is not a list")),
    }
}

/// Reject a record no store may persist: `seq < 1` or a non-finite `ts`.
pub(super) fn check_append(record: &TransitionRecord) -> Result<()> {
    if record.seq < 1 {
        return Err(StoreError::Other(format!("seq must be an int >= 1, got {}", record.seq)));
    }
    if !record.ts.is_finite() {
        return Err(StoreError::Other(format!("ts must be a finite number, got {}", record.ts)));
    }
    Ok(())
}

/// Reject a purge cutoff that would silently erase everything (`NaN` makes every comparison
/// false).
pub(super) fn check_cutoff(cutoff_ts: f64) -> Result<f64> {
    if cutoff_ts.is_nan() || cutoff_ts == f64::NEG_INFINITY {
        return Err(StoreError::Other(format!("cutoff_ts must not be NaN / -inf: {cutoff_ts}")));
    }
    Ok(cutoff_ts)
}

/// An append-only transition log, ordered per key.
///
/// Stores are safe to share between threads; [`SqliteLog`](super::SqliteLog) is also safe
/// across processes.
pub trait TransitionLogStore: Send + Sync + fmt::Debug {
    /// Persist `record` (its `seq` is taken as given).
    ///
    /// # Errors
    /// `seq < 1`, a non-finite `ts`, a duplicate `(machine_id, seq)` on SQLite, backend failure.
    fn append(&self, record: &TransitionRecord) -> Result<()>;

    /// Assign the next gap-free `seq` and append, returning the stored record. The default is
    /// `next_seq` + `append`; stores that can do it atomically override it, and writers racing
    /// on one key should use those.
    ///
    /// # Errors
    /// As [`append`](Self::append).
    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        let seq = self.next_seq(&record.machine_id)?;
        let stored = TransitionRecord { seq, ..record };
        self.append(&stored)?;
        Ok(stored)
    }

    /// The `seq` the next record for `machine_id` should carry (1 for an empty log).
    ///
    /// # Errors
    /// Backend failure or a corrupt log.
    fn next_seq(&self, machine_id: &str) -> Result<u64>;

    /// Records with `seq > after_seq`, ascending, at most `limit`.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for an unreadable record; backend failure.
    fn read(&self, machine_id: &str, after_seq: u64, limit: usize)
    -> Result<Vec<TransitionRecord>>;

    /// Delete records with `ts < cutoff_ts`; returns how many.
    ///
    /// # Errors
    /// A `NaN` / `-inf` cutoff, backend failure.
    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64>;

    /// Delete every record of `machine_id` (right to erasure); returns how many.
    ///
    /// # Errors
    /// Backend failure.
    fn forget(&self, machine_id: &str) -> Result<u64>;
}
