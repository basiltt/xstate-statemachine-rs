//! The receipt codec: one JSON shape and one HTTP-status mapping, identical to the Python
//! library's `receipts.py`.
//!
//! The idempotency inbox caches receipts so that a retried webhook gets the same answer as the
//! first delivery; web adapters turn a receipt into a response. Both need a stable wire form,
//! and it must be the one the Python runtime writes (ADR-0003), so an inbox row written by one
//! runtime is readable by the other.
//!
//! The wire form is `{"state_ids": [...], "changed": bool, "error": null | {"type", "message"},
//! "deferred": bool, "denied": bool, "duplicate": bool}`. `error` carries only the exception
//! class name and message, never anything reconstructable (no pickling on either side). On the
//! Rust side the class name is modelled as [`ReceiptErrorKind`]; the names Python writes are
//! preserved verbatim in the JSON `type` field.
//!
//! What is **not** ported: the Python `Receipt` is a named tuple with `deferred` / `denied`
//! flags set by the interpreter; [`xstate_core::Receipt`] has neither (it reports `unhandled`),
//! so [`ReceiptJson::from_receipt`] takes them as explicit [`ReceiptFlags`].

use serde_json::{Map, Value};
use xstate_core::{ErrorKind, Receipt, Status};

use crate::store::{Result, StoreError};

/// HTTP 200: a transition was taken, or the event was a clean no-op.
pub const STATUS_OK: u16 = 200;
/// HTTP 200: the event was handled nowhere in the current state, a clean no-op (not a 304).
/// Python `STATUS_NO_OP`, historically `STATUS_NOT_MODIFIED_OK`.
pub const STATUS_NO_OP: u16 = 200;
/// HTTP 202: the event was deferred by `onUnhandled: "defer"` (held, not run).
pub const STATUS_ACCEPTED: u16 = 202;
/// HTTP 409: a guard refused it, the first delivery is still in flight, or the instance is
/// finished / stopped.
pub const STATUS_CONFLICT: u16 = 409;
/// HTTP 500: an action, target or chain error while processing.
pub const STATUS_ERROR: u16 = 500;
/// HTTP 422: an idempotency key was reused with a different payload.
pub const STATUS_UNPROCESSABLE: u16 = 422;
/// HTTP 503: the idempotency inbox itself failed (`refuse` mode); the action did not run.
pub const STATUS_UNAVAILABLE: u16 = 503;

/// Classification of a receipt's `error`.
///
/// The first four are the client-side / policy errors that [`receipt_to_status`] maps to
/// dedicated statuses; everything else is [`Other`](Self::Other) with the Python class name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReceiptErrorKind {
    /// Python `IdempotencyMismatchError`: same key, different event (HTTP 422).
    IdempotencyMismatch,
    /// Python `IdempotencyInFlightError`: first delivery still running (HTTP 409).
    IdempotencyInFlight,
    /// Python `InboxUnavailableError`: the dedup store failed in `refuse` mode (HTTP 503).
    InboxUnavailable,
    /// Python `InterpreterStoppedError`: the instance is finished or stopped (HTTP 409).
    InterpreterStopped,
    /// Python `UnresolvedTargetError` (a subclass of `ActorSpawningError`): a `sendTo`,
    /// `forwardTo`, `sendParent` or `escalate` reached no live actor (HTTP 500: it is not in
    /// the client-error table, so the MRO fallback ends at the default).
    UnresolvedTarget,
    /// Any other error, carrying the exception class name as written in the JSON `type`.
    Other(String),
}

impl ReceiptErrorKind {
    /// The Python exception class name written to the JSON `type` field.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::IdempotencyMismatch => "IdempotencyMismatchError",
            Self::IdempotencyInFlight => "IdempotencyInFlightError",
            Self::InboxUnavailable => "InboxUnavailableError",
            Self::InterpreterStopped => "InterpreterStoppedError",
            Self::UnresolvedTarget => "UnresolvedTargetError",
            Self::Other(name) => name,
        }
    }

    /// Classify a JSON `type` string; unknown names become [`Other`](Self::Other).
    #[must_use]
    pub fn from_type_name(name: &str) -> Self {
        match name {
            "IdempotencyMismatchError" => Self::IdempotencyMismatch,
            "IdempotencyInFlightError" => Self::IdempotencyInFlight,
            "InboxUnavailableError" => Self::InboxUnavailable,
            "InterpreterStoppedError" => Self::InterpreterStopped,
            "UnresolvedTargetError" => Self::UnresolvedTarget,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The Python-spelled class name for an engine [`ErrorKind`].
    #[must_use]
    pub fn from_engine(kind: ErrorKind) -> Self {
        let name = match kind {
            ErrorKind::InvalidConfig => "InvalidConfigError",
            ErrorKind::RootTarget => "RootTargetError",
            ErrorKind::StateNotFound => "StateNotFoundError",
            ErrorKind::ImplementationMissing => "ImplementationMissingError",
            ErrorKind::InvalidEvent => "InvalidEventError",
            ErrorKind::UnknownEvent => "UnknownEventError",
            ErrorKind::InvalidLifecycle => "InterpreterStoppedError",
            ErrorKind::TransitionFailed => "TransitionFailedError",
            ErrorKind::RunawayChain => "RunawayChainError",
            ErrorKind::ActorSpawning => "ActorSpawningError",
            ErrorKind::UnresolvedTarget => "UnresolvedTargetError",
            ErrorKind::LimitExceeded => "LimitExceededError",
            ErrorKind::Snapshot => "SnapshotCorruptError",
            ErrorKind::Logic => "LogicError",
            _ => "XStateMachineError",
        };
        Self::from_type_name(name)
    }
}

/// The `error` of a receipt: the failure's class and message (nothing else survives a round
/// trip, by design).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptError {
    /// Failure class.
    pub kind: ReceiptErrorKind,
    /// `str(exception)`.
    pub message: String,
}

impl ReceiptError {
    /// Build an error of the given kind.
    #[must_use]
    pub fn new(kind: ReceiptErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    /// Classify an engine error the way the Python receipt would carry it.
    #[must_use]
    pub fn from_engine(error: &xstate_core::Error) -> Self {
        Self::new(ReceiptErrorKind::from_engine(error.kind()), error.to_string())
    }
}

/// The receipt flags the engine receipt does not carry; see the module notes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // mirrors the three Python receipt flags 1:1
pub struct ReceiptFlags {
    /// Held back by `onUnhandled: "defer"`.
    pub deferred: bool,
    /// Refused by a guard.
    pub denied: bool,
    /// Answered from the idempotency inbox instead of being processed.
    pub duplicate: bool,
}

/// A receipt in its Python JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // mirrors the Python receipt JSON 1:1
pub struct ReceiptJson {
    /// Active leaf ids after processing. Sorted on output.
    pub state_ids: Vec<String>,
    /// A transition was taken (or the context changed).
    pub changed: bool,
    /// Why the event did not run cleanly, if it did not.
    pub error: Option<ReceiptError>,
    /// Held for a later state.
    pub deferred: bool,
    /// Refused by a guard.
    pub denied: bool,
    /// Answered from the inbox; the receipt describes the original outcome.
    pub duplicate: bool,
}

impl ReceiptJson {
    /// Convert an engine receipt.
    ///
    /// `error` is the failure to report, if the caller has one. Two cases are filled in when it
    /// is `None`, so a refused event never reads as success: an event the machine rejected
    /// because it is `done` / `stopped` becomes [`ReceiptErrorKind::InterpreterStopped`]
    /// (HTTP 409), and a machine in the `error` status becomes
    /// [`Other`](ReceiptErrorKind::Other)`("XStateMachineError")` (HTTP 500).
    #[must_use]
    pub fn from_receipt(
        receipt: &Receipt,
        flags: ReceiptFlags,
        error: Option<ReceiptError>,
    ) -> Self {
        let error = error.or_else(|| match receipt.status {
            Status::Done | Status::Stopped if receipt.unhandled => Some(ReceiptError::new(
                ReceiptErrorKind::InterpreterStopped,
                format!("interpreter is {}; the event was not processed", receipt.status.as_str()),
            )),
            Status::Error => Some(ReceiptError::new(
                ReceiptErrorKind::Other("XStateMachineError".into()),
                "interpreter is in the error status",
            )),
            _ => None,
        });
        Self {
            state_ids: receipt.state_ids(),
            changed: receipt.changed,
            error,
            deferred: flags.deferred,
            denied: flags.denied,
            duplicate: flags.duplicate,
        }
    }

    /// The same receipt with `duplicate` set (what a cached answer looks like).
    #[must_use]
    pub fn into_duplicate(mut self) -> Self {
        self.duplicate = true;
        self
    }

    /// The JSON value, with `state_ids` sorted and the key order Python writes.
    #[must_use]
    pub fn to_value(&self) -> Value {
        self.to_value_with(true)
    }

    /// Like [`to_value`](Self::to_value), but with `include_messages == false` each error's
    /// message is written as `""` (type only), for a receipt shown to an untrusted client
    /// (Python `receipt_to_json(include_messages=False)`). The default keeps the message
    /// because the idempotency cache must replay the original answer faithfully.
    #[must_use]
    pub fn to_value_with(&self, include_messages: bool) -> Value {
        let mut ids = self.state_ids.clone();
        ids.sort();
        let error = self.error.as_ref().map_or(Value::Null, |e| {
            let message = if include_messages { e.message.clone() } else { String::new() };
            let mut m = Map::new();
            m.insert("type".into(), Value::String(e.kind.type_name().to_owned()));
            m.insert("message".into(), Value::String(message));
            Value::Object(m)
        });
        let mut map = Map::new();
        map.insert("state_ids".into(), Value::Array(ids.into_iter().map(Value::String).collect()));
        map.insert("changed".into(), Value::Bool(self.changed));
        map.insert("error".into(), error);
        map.insert("deferred".into(), Value::Bool(self.deferred));
        map.insert("denied".into(), Value::Bool(self.denied));
        map.insert("duplicate".into(), Value::Bool(self.duplicate));
        Value::Object(map)
    }

    /// The JSON text, formatted exactly like Python's `json.dumps(receipt_to_json(r))`
    /// (`", "` and `": "` separators, insertion key order, non-ASCII escaped).
    #[must_use]
    pub fn to_json(&self) -> String {
        pyjson::dumps(&self.to_value(), false, false)
    }

    /// [`to_json`](Self::to_json) with optional error-message omission; see
    /// [`to_value_with`](Self::to_value_with).
    #[must_use]
    pub fn to_json_with(&self, include_messages: bool) -> String {
        pyjson::dumps(&self.to_value_with(include_messages), false, false)
    }

    /// Validate and decode a receipt object (Python `receipt_from_json`).
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] when the value is not an object, `state_ids` is not a list of
    /// strings, `error` is neither `null` nor `{"type", "message"}` strings, or a flag is not a
    /// boolean. A cached receipt comes from outside the process, so it is never trusted.
    pub fn from_value(value: &Value) -> Result<Self> {
        let bad = |msg: &str| StoreError::Corrupt(format!("receipt record: {msg}"));
        let obj = value.as_object().ok_or_else(|| bad("must be an object"))?;
        let state_ids = obj
            .get("state_ids")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter().map(|v| v.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| bad("'state_ids' must be a list of strings"))?;
        let error = match obj.get("error") {
            None | Some(Value::Null) => None,
            Some(rec) => {
                let ty = rec.get("type").and_then(Value::as_str);
                let message = rec.get("message").and_then(Value::as_str);
                match (ty, message) {
                    (Some(ty), Some(message)) => {
                        Some(ReceiptError::new(ReceiptErrorKind::from_type_name(ty), message))
                    }
                    _ => {
                        return Err(bad("'error' must be null or {\"type\", \"message\"} strings"));
                    }
                }
            }
        };
        let flag = |key: &str| -> Result<bool> {
            match obj.get(key) {
                None => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(bad(&format!("'{key}' must be a boolean"))),
            }
        };
        Ok(Self {
            state_ids,
            changed: flag("changed")?,
            error,
            deferred: flag("deferred")?,
            denied: flag("denied")?,
            duplicate: flag("duplicate")?,
        })
    }

    /// Decode receipt JSON text.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for malformed JSON or a malformed record; see
    /// [`from_value`](Self::from_value).
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| StoreError::Corrupt(format!("receipt record is not JSON: {e}")))?;
        Self::from_value(&value)
    }
}

/// Map a receipt to an HTTP status, with the Python precedence (most severe first):
///
/// | Receipt                              | Status |
/// |:-------------------------------------|:------:|
/// | error: idempotency key mismatch      |  422   |
/// | error: idempotency key in flight     |  409   |
/// | error: inbox unavailable             |  503   |
/// | error: instance stopped / finished   |  409   |
/// | any other error                      |  500   |
/// | `deferred`                           |  202   |
/// | `denied`                             |  409   |
/// | otherwise (changed or clean no-op)   |  200   |
///
/// `duplicate` never changes the status: a cached receipt describes the original outcome and a
/// retry must get the same answer.
#[must_use]
pub fn receipt_to_status(receipt: &ReceiptJson) -> u16 {
    if let Some(error) = &receipt.error {
        return match error.kind {
            ReceiptErrorKind::IdempotencyMismatch => STATUS_UNPROCESSABLE,
            ReceiptErrorKind::IdempotencyInFlight | ReceiptErrorKind::InterpreterStopped => {
                STATUS_CONFLICT
            }
            ReceiptErrorKind::InboxUnavailable => STATUS_UNAVAILABLE,
            ReceiptErrorKind::UnresolvedTarget | ReceiptErrorKind::Other(_) => STATUS_ERROR,
        };
    }
    if receipt.deferred {
        return STATUS_ACCEPTED;
    }
    if receipt.denied {
        return STATUS_CONFLICT;
    }
    STATUS_NO_OP
}

/// Python-compatible JSON text (`json.dumps`), shared by the receipt, inbox and log codecs so
/// everything that lands in a database a Python process may read is byte-identical.
pub(crate) mod pyjson {
    use serde_json::{Number, Value};

    /// `json.dumps(value, sort_keys=sort_keys, separators=…, ensure_ascii=True)`; `compact`
    /// selects `(",", ":")` instead of the default `(", ", ": ")`.
    pub(crate) fn dumps(value: &Value, sort_keys: bool, compact: bool) -> String {
        let mut out = String::new();
        let (item, key) = if compact { (",", ":") } else { (", ", ": ") };
        write_value(&mut out, value, sort_keys, item, key);
        out
    }

    fn write_value(out: &mut String, value: &Value, sort_keys: bool, item: &str, key: &str) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&number(n)),
            Value::String(s) => write_str(out, s),
            Value::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(item);
                    }
                    write_value(out, v, sort_keys, item, key);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                if sort_keys {
                    entries.sort_by(|a, b| a.0.cmp(b.0));
                }
                out.push('{');
                for (i, (k, v)) in entries.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(item);
                    }
                    write_str(out, k);
                    out.push_str(key);
                    write_value(out, v, sort_keys, item, key);
                }
                out.push('}');
            }
        }
    }

    fn write_str(out: &mut String, s: &str) {
        use std::fmt::Write as _;
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (' '..='\u{7e}').contains(&c) || c == '\u{7f}' => out.push(c),
                c => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.push('"');
    }

    fn number(n: &Number) -> String {
        if let Some(i) = n.as_i64() {
            i.to_string()
        } else if let Some(u) = n.as_u64() {
            u.to_string()
        } else {
            n.as_f64().map_or_else(|| n.to_string(), float_repr)
        }
    }

    /// Python's `float.__repr__`: shortest round-trip digits, fixed notation for decimal
    /// exponents in `-4..16`, otherwise `d.ddde±XX`.
    pub(crate) fn float_repr(x: f64) -> String {
        if x == 0.0 {
            return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
        }
        let sci = format!("{x:e}");
        let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let (sign, mantissa) = mantissa.strip_prefix('-').map_or(("", mantissa), |m| ("-", m));
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        if (-4..16).contains(&exp) {
            let (int, frac) = if exp >= 0 {
                let int_len = exp.unsigned_abs() as usize + 1;
                if digits.len() <= int_len {
                    (format!("{digits}{}", "0".repeat(int_len - digits.len())), "0".to_owned())
                } else {
                    (digits[..int_len].to_owned(), digits[int_len..].to_owned())
                }
            } else {
                ("0".to_owned(), format!("{}{digits}", "0".repeat(exp.unsigned_abs() as usize - 1)))
            };
            format!("{sign}{int}.{frac}")
        } else {
            let mant = if digits.len() == 1 {
                digits
            } else {
                format!("{}.{}", &digits[..1], &digits[1..])
            };
            let esign = if exp < 0 { '-' } else { '+' };
            format!("{sign}{mant}e{esign}{:02}", exp.unsigned_abs())
        }
    }
}
