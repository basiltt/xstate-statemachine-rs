//! The [`DeadLetter`] record (Python `patterns.dead_letter.DeadLetter`), shared by the chart
//! plugin and the dispatcher so one store and one CLI serve both.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{EdaError, Result};

/// One entry of a dead letter's error chain, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ErrorEntry {
    /// `"service"`, `"action"` or `"dispatcher"`.
    #[serde(default)]
    pub source: String,
    /// Service / action name, or the dispatcher reason.
    #[serde(default)]
    pub name: String,
    /// Python-spelled error class name.
    #[serde(rename = "type", default)]
    pub ty: String,
    /// Error message (never the error object).
    #[serde(default)]
    pub message: String,
}

impl ErrorEntry {
    /// Build an entry.
    pub fn new(
        source: impl Into<String>,
        name: impl Into<String>,
        ty: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self { source: source.into(), name: name.into(), ty: ty.into(), message: message.into() }
    }
}

/// One captured poison message. Field order and JSON shape are Python's `asdict()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeadLetter {
    /// Instance (or machine) id.
    pub machine_id: String,
    /// The dead-letter state that was entered (`""` for dispatcher records).
    pub state_id: String,
    /// The last event, `{"type", "payload"}` with the payload redacted.
    pub event: Value,
    /// The attempt counter at capture.
    pub attempts: Option<i64>,
    /// The error chain since the last clean step, oldest first.
    pub errors: Vec<ErrorEntry>,
    /// The redacted snapshot (`{}` when omitted).
    pub snapshot: Value,
    /// Epoch seconds.
    pub taken_at: f64,
    /// Record id; for a dead-lettered envelope the envelope id, so a replay re-uses it and an
    /// inbox dedups a double replay.
    #[serde(default)]
    pub id: String,
    /// `"dead_letter_state"`, `"max_attempts"`, `"unknown_event"`, `"corrupt"`, ...
    #[serde(default = "default_reason")]
    pub reason: String,
    /// The redacted envelope (EDA dead letters).
    #[serde(default)]
    pub envelope: Option<Value>,
    /// The topic the envelope came from.
    #[serde(default)]
    pub topic: Option<String>,
    /// Structure hash of the machine that failed; replay refuses another one unless forced.
    #[serde(default)]
    pub machine_hash: Option<String>,
    /// The chart `version` at capture.
    #[serde(default)]
    pub machine_version: Option<String>,
    /// Epoch seconds a replay resolved it.
    #[serde(default)]
    pub resolved_at: Option<f64>,
}

fn default_reason() -> String {
    "dead_letter_state".into()
}

impl DeadLetter {
    /// A record with the defaults of the optional fields and a fresh hex UUID4 id.
    #[must_use]
    pub fn new(
        machine_id: impl Into<String>,
        state_id: impl Into<String>,
        event: Value,
        taken_at: f64,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            state_id: state_id.into(),
            event,
            attempts: None,
            errors: Vec::new(),
            snapshot: Value::Object(Map::new()),
            taken_at,
            id: uuid::Uuid::new_v4().simple().to_string(),
            reason: default_reason(),
            envelope: None,
            topic: None,
            machine_hash: None,
            machine_version: None,
            resolved_at: None,
        }
    }

    /// The record as a JSON object (Python `to_dict`, field order preserved).
    #[must_use]
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Python `to_json`: `json.dumps(asdict, sort_keys=True)` with the default separators.
    #[must_use]
    pub fn to_json(&self) -> String {
        xstate_core::pyjson::dumps(&self.to_dict())
    }

    /// Build a record from a JSON object; unknown keys are ignored and an empty id is replaced.
    ///
    /// # Errors
    /// [`EdaError::InvalidArgument`] when a required field is missing or mistyped.
    pub fn from_dict(raw: &Value) -> Result<Self> {
        let mut rec: Self = serde_json::from_value(raw.clone())
            .map_err(|e| EdaError::InvalidArgument(format!("invalid dead letter: {e}")))?;
        if rec.id.is_empty() {
            rec.id = uuid::Uuid::new_v4().simple().to_string();
        }
        Ok(rec)
    }
}
