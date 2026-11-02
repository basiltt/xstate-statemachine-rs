//! Envelope validation, in the exact order and with the exact messages of the Python
//! `Envelope.validate()`.

use serde_json::{Map, Value};

use super::{ATTEMPT_EXTENSION, CORE_ATTRIBUTES, FORBIDDEN_EXTENSIONS, KNOWN_EXTENSIONS};
use crate::error::EnvelopeError;
use crate::py::repr_str;

/// Longest accepted attribute, in characters.
pub(crate) const MAX_ATTR: usize = 1024;

/// CloudEvents data nested deeper than this is refused (Python `MAX_DATA_DEPTH`): every later
/// consumer (redaction, the dead-letter writer) walks it recursively.
pub const MAX_DATA_DEPTH: usize = 64;

/// The parts of an envelope as decoded JSON values, before typing.
pub(crate) struct RawParts<'a> {
    /// Core and known-extension attributes that were present (`data` excluded).
    pub attrs: &'a Map<String, Value>,
    /// `data` (`Null` when absent).
    pub data: &'a Value,
    /// Further extensions.
    pub extensions: &'a Map<String, Value>,
}

/// `EnvelopeCorruptError` unless the parts form a valid envelope (Python `validate()`).
pub(crate) fn validate_parts(parts: &RawParts<'_>) -> Result<(), EnvelopeError> {
    check_depth(parts.data)?;
    let spec = parts.attrs.get("specversion").unwrap_or(&Value::Null);
    if spec.as_str() != Some(super::SPECVERSION) {
        return Err(EnvelopeError::corrupt(format!(
            "unsupported specversion {}",
            crate::py::repr_value(spec)
        )));
    }
    for name in ["id", "type", "source"] {
        check_str(name, parts.attrs.get(name), true)?;
    }
    for name in ["subject", "time", "datacontenttype"].into_iter().chain(KNOWN_EXTENSIONS) {
        check_str(name, parts.attrs.get(name), false)?;
    }
    check_extensions(parts.extensions)
}

fn check_str(name: &str, value: Option<&Value>, required: bool) -> Result<(), EnvelopeError> {
    let value = match value {
        None | Some(Value::Null) if required => {
            return Err(EnvelopeError::corrupt(format!(
                "envelope attribute {} missing",
                repr_str(name)
            )));
        }
        None | Some(Value::Null) => return Ok(()),
        Some(v) => v,
    };
    let text = match value.as_str() {
        Some(s) if !(required && s.is_empty()) => s,
        _ => {
            return Err(EnvelopeError::corrupt(format!(
                "envelope attribute {} must be a non-empty string",
                repr_str(name)
            )));
        }
    };
    if text.chars().count() > MAX_ATTR {
        return Err(EnvelopeError::corrupt(format!(
            "envelope attribute {} exceeds {MAX_ATTR} characters",
            repr_str(name)
        )));
    }
    Ok(())
}

/// `true` when an extension name looks credential-bearing (X0.8).
pub(crate) fn is_forbidden(name: &str) -> bool {
    let low = name.to_lowercase().replace(['-', '_'], "");
    FORBIDDEN_EXTENSIONS.iter().any(|bad| low.contains(bad))
}

/// CloudEvents 1.0: lower-case ASCII letters or digits, 1 to 20 of them.
///
/// Python's `re.match(r"^[a-z0-9]{1,20}$")` also accepts one trailing newline; that quirk is
/// deliberately not reproduced.
pub(crate) fn is_extension_name(name: &str) -> bool {
    (1..=20).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn check_extensions(ext: &Map<String, Value>) -> Result<(), EnvelopeError> {
    for (name, value) in ext {
        if !is_extension_name(name) {
            return Err(EnvelopeError::corrupt(format!(
                "invalid CloudEvents extension name {} (lower-case letters/digits, at most 20)",
                repr_str(name)
            )));
        }
        if CORE_ATTRIBUTES.contains(&name.as_str()) || KNOWN_EXTENSIONS.contains(&name.as_str()) {
            return Err(EnvelopeError::corrupt(format!(
                "{} is an envelope attribute, not an extension",
                repr_str(name)
            )));
        }
        if is_forbidden(name) {
            return Err(EnvelopeError::corrupt(format!(
                "extension {} looks credential-bearing; secrets never travel in envelope \
                 extensions (X0.8)",
                repr_str(name)
            )));
        }
        let is_int = value.as_i64().is_some() || value.as_u64().is_some();
        if !(value.is_string() || value.is_boolean() || is_int) {
            return Err(EnvelopeError::corrupt(format!(
                "extension {} must be a string, integer or boolean",
                repr_str(name)
            )));
        }
        if name == "traceparent" && !is_traceparent(&py_str(value)) {
            return Err(EnvelopeError::corrupt("invalid W3C traceparent extension"));
        }
        if name == ATTEMPT_EXTENSION && value.as_u64().is_none() {
            return Err(EnvelopeError::corrupt(format!(
                "{ATTEMPT_EXTENSION} must be a non-negative integer"
            )));
        }
    }
    Ok(())
}

/// Python `str()` of an extension value.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// W3C Trace Context `traceparent`, version `00`-style: `vv-<32 hex>-<16 hex>-ff`, lower-case
/// hex, version `ff` and all-zero ids invalid.
pub(crate) fn is_traceparent(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    let hex = |s: &str, n: usize| {
        s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    let [version, trace, span, flags] = parts.as_slice() else { return false };
    hex(version, 2)
        && *version != "ff"
        && hex(trace, 32)
        && trace.bytes().any(|b| b != b'0')
        && hex(span, 16)
        && span.bytes().any(|b| b != b'0')
        && hex(flags, 2)
}

/// `EnvelopeCorruptError` when `value` nests deeper than [`MAX_DATA_DEPTH`] (iterative).
pub(crate) fn check_depth(value: &Value) -> Result<(), EnvelopeError> {
    let mut stack = vec![(value, 0usize)];
    while let Some((item, depth)) = stack.pop() {
        if depth > MAX_DATA_DEPTH {
            return Err(EnvelopeError::corrupt(format!(
                "envelope data is nested deeper than {MAX_DATA_DEPTH}"
            )));
        }
        match item {
            Value::Object(map) => stack.extend(map.values().map(|v| (v, depth + 1))),
            Value::Array(items) => stack.extend(items.iter().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_grammar() {
        let ok = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(is_traceparent(ok));
        assert!(!is_traceparent(&ok.replacen("00", "ff", 1)));
        assert!(!is_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"));
        assert!(!is_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"));
        assert!(!is_traceparent(&ok.to_uppercase()));
        assert!(!is_traceparent(&format!("{ok}-x")));
    }

    #[test]
    fn extension_names_and_forbidden() {
        assert!(is_extension_name("tenant1"));
        assert!(!is_extension_name("Tenant"));
        assert!(!is_extension_name(""));
        assert!(!is_extension_name("tenant\n"));
        assert!(is_forbidden("x-Api_Key") && is_forbidden("authtoken") && !is_forbidden("tenant"));
    }

    #[test]
    fn depth_limit_counts_leaves() {
        let mut v = Value::from(1);
        for _ in 0..MAX_DATA_DEPTH {
            v = Value::Array(vec![v]);
        }
        assert!(check_depth(&v).is_ok());
        assert!(check_depth(&Value::Array(vec![v])).is_err());
    }
}
