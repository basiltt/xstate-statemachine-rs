//! [`Envelope`]: the CloudEvents 1.0 wire format every broker carries (Python `eda/envelope.py`).
//!
//! One envelope for every broker, CloudEvents 1.0 from day one: `specversion, id, type, source,
//! subject, time, datacontenttype, data` plus the extensions `correlationid`, `causationid`,
//! `machineid`, `machineversion`, so a non-Rust (or non-Python) consumer can read it. `subject`
//! is the machine instance key **and** the partition key.
//!
//! * [`Envelope::from_json`] checks the byte size *before* parsing (same cap as snapshots,
//!   [`DEFAULT_MAX_SNAPSHOT_BYTES`]), then the shape; a wrong shape is
//!   [`EnvelopeError::Corrupt`].
//! * Extension names that would carry credentials (`authorization`, `cookie`, ...) are refused;
//!   [`Envelope::safe_extensions`] drops them from transport headers. `traceparent` must match
//!   the W3C Trace Context grammar.
//! * [`Envelope::to_json`] is byte-identical to Python's `to_json` (compact separators,
//!   `ensure_ascii`, the same key order).

use serde_json::{Map, Value};
use xstate_core::{Event, Machine};
pub use xstate_persistence::DEFAULT_MAX_SNAPSHOT_BYTES;

use crate::error::EnvelopeError;
use crate::py::{is_falsy, type_name};

mod id;
mod validate;

pub use id::{new_id, new_id_at};
pub use validate::MAX_DATA_DEPTH;
pub(crate) use validate::is_forbidden;
use validate::{RawParts, validate_parts};

/// The CloudEvents version every envelope carries.
pub const SPECVERSION: &str = "1.0";
/// The per-envelope delivery-attempt counter extension (X0.8).
pub const ATTEMPT_EXTENSION: &str = "xsmattempt";
/// Substrings that mark an extension as credential-bearing (X0.8).
pub const FORBIDDEN_EXTENSIONS: [&str; 6] =
    ["authorization", "cookie", "password", "secret", "token", "apikey"];
/// Default `datacontenttype`.
pub const DEFAULT_CONTENT_TYPE: &str = "application/json";

/// CloudEvents core attributes, in `to_dict` order.
pub(crate) const CORE_ATTRIBUTES: [&str; 8] =
    ["specversion", "id", "type", "source", "subject", "time", "datacontenttype", "data"];
/// The extensions modelled as attributes, in `to_dict` order.
pub(crate) const KNOWN_EXTENSIONS: [&str; 4] =
    ["correlationid", "causationid", "machineid", "machineversion"];

/// `xsm.<machine>.<EVENT>` → `EVENT`; anything else unchanged (Python `default_event_name`).
///
/// ```
/// assert_eq!(xstate_eda::default_event_name("xsm.order.PAY"), "PAY");
/// assert_eq!(xstate_eda::default_event_name("order.paid"), "order.paid");
/// ```
#[must_use]
pub fn default_event_name(envelope_type: &str) -> &str {
    if let Some(rest) = envelope_type.strip_prefix("xsm.")
        && let Some((_, event)) = rest.split_once('.')
        && !event.is_empty()
    {
        return event;
    }
    envelope_type
}

/// A CloudEvents 1.0 event as the library moves it between machines.
///
/// Build one with [`Envelope::new`] (fresh `id` and `time`) or [`Envelope::from_transition`];
/// decode with [`Envelope::from_json`] / [`Envelope::from_dict`]. Fields are public for
/// reading; code that mutates them should call [`Envelope::validate`] again (the dispatcher
/// does).
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// Event type, e.g. `xsm.order.PAY` (inbound command) or `order.paid` (integration event).
    pub ty: String,
    /// URI-reference of the producer (`xsm/order`).
    pub source: String,
    /// Machine instance key, also the partition key.
    pub subject: Option<String>,
    /// Sortable unique id ([`new_id`]); the idempotency key.
    pub id: String,
    /// RFC 3339 timestamp.
    pub time: Option<String>,
    /// JSON payload; must be an object to become an [`Event`]. `Null` means absent.
    pub data: Value,
    /// Always [`SPECVERSION`].
    pub specversion: String,
    /// Content type of `data` (default `application/json`).
    pub datacontenttype: Option<String>,
    /// Correlation-chain extension shared by every envelope of one business flow.
    pub correlationid: Option<String>,
    /// Id of the envelope that caused this one.
    pub causationid: Option<String>,
    /// The producing machine.
    pub machineid: Option<String>,
    /// The producing machine's chart `version`.
    pub machineversion: Option<String>,
    /// Further CloudEvents extensions (validated: names, value types, no credentials).
    pub extensions: Map<String, Value>,
}

/// Where an outbound envelope comes from, for [`Envelope::from_transition`].
#[derive(Debug, Clone, Copy)]
pub struct TransitionOrigin<'a> {
    /// The machine that transitioned.
    pub machine: &'a Machine,
    /// The instance's store key (Python `interpreter.store_key`); the machine id when `None`.
    pub instance_key: Option<&'a str>,
    /// Explicit subject; default the cause's subject, then the instance key.
    pub subject: Option<&'a str>,
    /// Explicit source; default `xsm/<machine id>`.
    pub source: Option<&'a str>,
}

impl<'a> TransitionOrigin<'a> {
    /// An origin for `machine` with every default.
    #[must_use]
    pub fn new(machine: &'a Machine) -> Self {
        Self { machine, instance_key: None, subject: None, source: None }
    }
}

/// Builder returned by [`Envelope::new`] and [`Envelope::builder`].
#[derive(Debug, Clone)]
#[must_use]
pub struct EnvelopeBuilder {
    env: Envelope,
}

impl EnvelopeBuilder {
    /// Set the subject (instance / partition key).
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.env.subject = Some(subject.into());
        self
    }
    /// Set the payload.
    pub fn data(mut self, data: Value) -> Self {
        self.env.data = data;
        self
    }
    /// Set the source.
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.env.source = source.into();
        self
    }
    /// Set the id (default: a fresh [`new_id`]).
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.env.id = id.into();
        self
    }
    /// Set (or clear) the time.
    pub fn time(mut self, time: Option<String>) -> Self {
        self.env.time = time;
        self
    }
    /// Set (or clear) the content type.
    pub fn datacontenttype(mut self, value: Option<String>) -> Self {
        self.env.datacontenttype = value;
        self
    }
    /// Set the correlation id.
    pub fn correlationid(mut self, value: impl Into<String>) -> Self {
        self.env.correlationid = Some(value.into());
        self
    }
    /// Set the causation id.
    pub fn causationid(mut self, value: impl Into<String>) -> Self {
        self.env.causationid = Some(value.into());
        self
    }
    /// Set the producing machine id.
    pub fn machineid(mut self, value: impl Into<String>) -> Self {
        self.env.machineid = Some(value.into());
        self
    }
    /// Set (or clear) the producing machine version.
    pub fn machineversion(mut self, value: Option<String>) -> Self {
        self.env.machineversion = value;
        self
    }
    /// Add an extension.
    pub fn extension(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.env.extensions.insert(name.into(), value.into());
        self
    }

    /// Validate and return the envelope (an empty id is replaced by [`new_id`]).
    ///
    /// # Errors
    /// [`EnvelopeError::Corrupt`] when the shape is invalid.
    pub fn build(mut self) -> Result<Envelope, EnvelopeError> {
        if self.env.id.is_empty() {
            self.env.id = new_id();
        }
        self.env.validate()?;
        Ok(self.env)
    }
}

impl Envelope {
    /// A fresh envelope of type `ty`: source `xsm`, a new sortable id and `time` now
    /// (Python `Envelope.new`). Finish with [`EnvelopeBuilder::build`].
    // Named after Python's `Envelope.new`, which this mirrors; it returns the builder that
    // finishes the envelope.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(ty: impl Into<String>) -> EnvelopeBuilder {
        Self::builder(ty, "xsm").time(Some(crate::py::now_iso()))
    }

    /// An envelope with only `type` and `source` set (the Python dataclass constructor: no
    /// `time` unless given).
    pub fn builder(ty: impl Into<String>, source: impl Into<String>) -> EnvelopeBuilder {
        EnvelopeBuilder {
            env: Self {
                ty: ty.into(),
                source: source.into(),
                subject: None,
                id: String::new(),
                time: None,
                data: Value::Null,
                specversion: SPECVERSION.to_owned(),
                datacontenttype: Some(DEFAULT_CONTENT_TYPE.to_owned()),
                correlationid: None,
                causationid: None,
                machineid: None,
                machineversion: None,
                extensions: Map::new(),
            },
        }
    }

    /// An outbound envelope describing what a machine just did (Python `from_transition`).
    ///
    /// `causationid` is the id of the inbound `cause` (if any) and the correlation id is
    /// inherited from it, so a chain of machines shares one `correlationid`; the cause's
    /// `traceparent` is propagated.
    ///
    /// # Errors
    /// [`EnvelopeError::Corrupt`] when the result is invalid (e.g. `data` too deep).
    pub fn from_transition(
        origin: &TransitionOrigin<'_>,
        ty: impl Into<String>,
        data: Value,
        cause: Option<&Envelope>,
    ) -> Result<Self, EnvelopeError> {
        let machine = origin.machine;
        let subject = origin
            .subject
            .filter(|s| !s.is_empty())
            .or_else(|| cause.and_then(|c| c.subject.as_deref()).filter(|s| !s.is_empty()))
            .map_or_else(
                || origin.instance_key.filter(|k| !k.is_empty()).unwrap_or(machine.id()).to_owned(),
                str::to_owned,
            );
        let id = new_id();
        let correlation = match cause {
            Some(c) => c.correlationid.clone().unwrap_or_else(|| c.id.clone()),
            None => id.clone(),
        };
        let mut builder = Self::builder(
            ty,
            origin.source.map_or_else(|| format!("xsm/{}", machine.id()), str::to_owned),
        )
        .subject(subject)
        .id(id)
        .time(Some(crate::py::now_iso()))
        .data(data)
        .correlationid(correlation)
        .machineid(machine.id())
        .machineversion(machine.version().filter(|v| !v.is_empty()).map(str::to_owned));
        if let Some(c) = cause {
            builder = builder.causationid(c.id.clone());
            if let Some(tp) = c.extensions.get("traceparent") {
                builder = builder.extension("traceparent", tp.clone());
            }
        }
        builder.build()
    }

    /// Raise [`EnvelopeError::Corrupt`] unless the shape is valid.
    ///
    /// # Errors
    /// See [`EnvelopeError::Corrupt`].
    pub fn validate(&self) -> Result<&Self, EnvelopeError> {
        let attrs = self.attribute_map();
        validate_parts(&RawParts {
            attrs: &attrs,
            data: &self.data,
            extensions: &self.extensions,
        })?;
        Ok(self)
    }

    /// Delivery attempts recorded so far (`0` for a first delivery).
    #[must_use]
    pub fn attempt(&self) -> u64 {
        self.extensions.get(ATTEMPT_EXTENSION).and_then(Value::as_u64).unwrap_or(0)
    }

    /// A copy with the attempt extension set to `n`.
    #[must_use]
    pub fn with_attempt(&self, n: u64) -> Self {
        let mut out = self.clone();
        out.extensions.insert(ATTEMPT_EXTENSION.to_owned(), Value::from(n));
        out
    }

    /// The [`Event`] this envelope delivers to a machine: named `event_type` (when given and
    /// non-empty) or [`default_event_name`]`(type)`, with `data` as payload (`{}` when absent).
    ///
    /// # Errors
    /// [`EnvelopeError::Corrupt`] when the envelope is invalid or `data` is not an object.
    pub fn to_event(&self, event_type: Option<&str>) -> Result<Event, EnvelopeError> {
        self.validate()?;
        let payload = match &self.data {
            Value::Null => Value::Object(Map::new()),
            Value::Object(map) => Value::Object(map.clone()),
            other => {
                return Err(EnvelopeError::corrupt(format!(
                    "envelope data must be a JSON object to become an event payload, got {}",
                    type_name(other)
                )));
            }
        };
        let name =
            event_type.filter(|t| !t.is_empty()).unwrap_or_else(|| default_event_name(&self.ty));
        Ok(Event::with_payload(name, payload))
    }

    fn attribute_map(&self) -> Map<String, Value> {
        let opt = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        let mut out = Map::new();
        out.insert("specversion".into(), Value::String(self.specversion.clone()));
        out.insert("id".into(), Value::String(self.id.clone()));
        out.insert("type".into(), Value::String(self.ty.clone()));
        out.insert("source".into(), Value::String(self.source.clone()));
        out.insert("subject".into(), opt(&self.subject));
        out.insert("time".into(), opt(&self.time));
        out.insert("datacontenttype".into(), opt(&self.datacontenttype));
        out.insert("data".into(), self.data.clone());
        out.insert("correlationid".into(), opt(&self.correlationid));
        out.insert("causationid".into(), opt(&self.causationid));
        out.insert("machineid".into(), opt(&self.machineid));
        out.insert("machineversion".into(), opt(&self.machineversion));
        out
    }

    /// The CloudEvents structured-mode JSON object, absent attributes omitted, in Python's
    /// key order (core attributes, known extensions, then further extensions).
    #[must_use]
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut out: Map<String, Value> =
            self.attribute_map().into_iter().filter(|(_, v)| !v.is_null()).collect();
        for (k, v) in &self.extensions {
            out.insert(k.clone(), v.clone());
        }
        out
    }

    /// Compact JSON, byte-identical to Python's `to_json` (ASCII-escaped).
    ///
    /// # Errors
    /// [`EnvelopeError::TooLarge`] when the text exceeds `max_bytes`.
    pub fn to_json(&self, max_bytes: usize) -> Result<String, EnvelopeError> {
        let text = xstate_core::pyjson::dumps_compact(&Value::Object(self.to_dict()));
        if text.len() > max_bytes {
            return Err(EnvelopeError::TooLarge(format!(
                "envelope {} is larger than {max_bytes} bytes",
                self.id
            )));
        }
        Ok(text)
    }

    /// Build an envelope from a decoded JSON object; keys beyond the known attributes become
    /// extensions. A missing `datacontenttype` defaults to `application/json`; a falsy `id`
    /// (`null`, `""`) is replaced by a fresh one, as in Python.
    ///
    /// # Errors
    /// [`EnvelopeError::Corrupt`] when `raw` is not an object, lacks `id`, `type`, `source` or
    /// `specversion`, or fails validation.
    pub fn from_dict(raw: &Value) -> Result<Self, EnvelopeError> {
        let Value::Object(map) = raw else {
            return Err(EnvelopeError::corrupt("an envelope must be a JSON object"));
        };
        let mut attrs = Map::new();
        let mut extensions = Map::new();
        let mut data = Value::Null;
        for (k, v) in map {
            if k == "data" {
                data = v.clone();
            } else if CORE_ATTRIBUTES.contains(&k.as_str())
                || KNOWN_EXTENSIONS.contains(&k.as_str())
            {
                attrs.insert(k.clone(), v.clone());
            } else {
                extensions.insert(k.clone(), v.clone());
            }
        }
        for required in ["id", "type", "source", "specversion"] {
            if !attrs.contains_key(required) {
                return Err(EnvelopeError::corrupt(format!(
                    "envelope attribute {} missing",
                    crate::py::repr_str(required)
                )));
            }
        }
        if attrs.get("id").is_none_or(is_falsy) {
            attrs.insert("id".into(), Value::String(new_id()));
        }
        if !attrs.contains_key("datacontenttype") {
            attrs.insert("datacontenttype".into(), Value::from(DEFAULT_CONTENT_TYPE));
        }
        validate_parts(&RawParts { attrs: &attrs, data: &data, extensions: &extensions })?;
        Ok(Self::from_valid(&attrs, data, extensions))
    }

    fn from_valid(attrs: &Map<String, Value>, data: Value, extensions: Map<String, Value>) -> Self {
        let text = |k: &str| attrs.get(k).and_then(Value::as_str).map(str::to_owned);
        Self {
            ty: text("type").unwrap_or_default(),
            source: text("source").unwrap_or_default(),
            subject: text("subject"),
            id: text("id").unwrap_or_default(),
            time: text("time"),
            data,
            specversion: text("specversion").unwrap_or_default(),
            datacontenttype: text("datacontenttype"),
            correlationid: text("correlationid"),
            causationid: text("causationid"),
            machineid: text("machineid"),
            machineversion: text("machineversion"),
            extensions,
        }
    }

    /// Decode a structured-mode envelope; the size is checked before parsing.
    ///
    /// # Errors
    /// [`EnvelopeError::TooLarge`] over `max_bytes`; [`EnvelopeError::Corrupt`] when the text
    /// is not JSON (`NaN` / `Infinity` included), nests too deeply or has the wrong shape.
    pub fn from_json(text: &str, max_bytes: usize) -> Result<Self, EnvelopeError> {
        Self::from_json_bytes(text.as_bytes(), max_bytes)
    }

    /// [`Envelope::from_json`] for raw bytes (must be UTF-8 JSON).
    ///
    /// # Errors
    /// As [`Envelope::from_json`].
    pub fn from_json_bytes(bytes: &[u8], max_bytes: usize) -> Result<Self, EnvelopeError> {
        if bytes.len() > max_bytes {
            return Err(EnvelopeError::TooLarge(format!(
                "envelope is {} bytes; the limit is {max_bytes}",
                bytes.len()
            )));
        }
        let raw: Value = serde_json::from_slice(bytes).map_err(|e| {
            if e.to_string().contains("recursion limit") {
                EnvelopeError::corrupt("envelope is nested too deeply")
            } else {
                EnvelopeError::corrupt(format!("envelope is not JSON: {e}"))
            }
        })?;
        Self::from_dict(&raw)
    }

    /// `raw` minus credential-bearing names (X0.8), for decoders that copy transport headers
    /// into extensions.
    #[must_use]
    pub fn safe_extensions(raw: &Map<String, Value>) -> Map<String, Value> {
        raw.iter().filter(|(k, _)| !is_forbidden(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_fills_id_and_time_and_round_trips() {
        let env = Envelope::new("xsm.o.GO").subject("o-1").data(json!({"n": 1})).build().unwrap();
        assert_eq!(env.id.len(), 36);
        assert!(env.time.as_deref().is_some_and(|t| t.ends_with('Z')));
        let text = env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap();
        assert_eq!(Envelope::from_json(&text, DEFAULT_MAX_SNAPSHOT_BYTES).unwrap(), env);
        let ev = env.to_event(None).unwrap();
        assert_eq!(ev.ty, "GO");
        assert_eq!(ev.payload, json!({"n": 1}));
    }

    #[test]
    fn size_cap_before_parsing() {
        let err = Envelope::from_json("{not json at all}", 4).unwrap_err();
        assert!(matches!(err, EnvelopeError::TooLarge(_)), "{err}");
        let env = Envelope::builder("t", "s").data(json!({"x": "y".repeat(100)})).build().unwrap();
        assert!(matches!(env.to_json(50), Err(EnvelopeError::TooLarge(_))));
    }

    #[test]
    fn non_json_and_deep_json_are_corrupt() {
        assert!(matches!(Envelope::from_json("{\"a\": NaN}", 100), Err(EnvelopeError::Corrupt(_))));
        let deep = format!("{}{}", "[".repeat(500), "]".repeat(500));
        let err = Envelope::from_json(&deep, 10_000).unwrap_err();
        assert_eq!(err.to_string(), "envelope is nested too deeply");
    }

    #[test]
    fn attempts_and_events() {
        let env = Envelope::builder("t", "s").data(json!([1])).build().unwrap();
        assert_eq!(env.attempt(), 0);
        assert_eq!(env.with_attempt(3).attempt(), 3);
        let err = env.to_event(None).unwrap_err();
        assert!(err.to_string().ends_with("got list"), "{err}");
        assert!(env.to_event(Some("X")).is_err());
        let ok = Envelope::builder("t", "s").build().unwrap();
        assert_eq!(ok.to_event(Some("X")).unwrap().payload, json!({}));
    }

    #[test]
    fn safe_extensions_drop_credentials() {
        let raw = json!({"tenant": "t", "x-api-key": "k", "Cookie": "c"});
        let safe = Envelope::safe_extensions(raw.as_object().unwrap());
        assert_eq!(Value::Object(safe), json!({"tenant": "t"}));
    }
}
