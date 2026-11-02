//! Small Python-compatibility helpers: `repr()` of strings and JSON values, Python type names,
//! the wall clock and RFC 3339 timestamps.

use std::fmt::Write as _;

use serde_json::Value;

/// Python `repr()` of a `str`: single quotes unless the text contains `'` and no `"`.
pub(crate) fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of an optional string (`None` when absent).
pub(crate) fn repr_opt(s: Option<&str>) -> String {
    s.map_or_else(|| "None".to_owned(), repr_str)
}

/// Python `repr()` of a decoded JSON value (close enough for error messages).
pub(crate) fn repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => repr_str(s),
        Value::Array(items) => {
            format!("[{}]", items.iter().map(repr_value).collect::<Vec<_>>().join(", "))
        }
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python `type(value).__name__` for a decoded JSON value.
pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness of a decoded JSON value.
pub(crate) fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
    }
}

/// Epoch seconds from the system clock (Python `time.time()`).
#[must_use]
pub fn wall_now() -> f64 {
    xstate_persistence::store::wall_now()
}

/// Shared wall clock type for stores whose leases or retention tests need a fake clock.
pub type WallClock = std::sync::Arc<dyn Fn() -> f64 + Send + Sync>;

/// The default [`WallClock`]: [`wall_now`].
pub(crate) fn system_clock() -> WallClock {
    std::sync::Arc::new(wall_now)
}

/// `datetime.now(timezone.utc).isoformat()` with `+00:00` spelled `Z`: microsecond precision,
/// the fraction omitted when it is zero (exactly as Python does).
#[must_use]
pub fn now_iso() -> String {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    iso_from_unix(now.as_secs(), now.subsec_micros())
}

/// Format a UTC instant as Python's `isoformat()` does, with a `Z` suffix.
pub(crate) fn iso_from_unix(secs: u64, micros: u32) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if micros == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{micros:06}Z")
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_matches_python() {
        assert_eq!(iso_from_unix(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_719_835_200, 123_456), "2024-07-01T12:00:00.123456Z");
        assert_eq!(iso_from_unix(951_782_400, 5), "2000-02-29T00:00:00.000005Z");
        assert!(now_iso().ends_with('Z'));
    }

    #[test]
    fn reprs() {
        assert_eq!(repr_str("a'b"), "\"a'b\"");
        assert_eq!(repr_opt(None), "None");
        assert_eq!(repr_value(&json!(["x", 1, null, true])), "['x', 1, None, True]");
        assert_eq!(type_name(&json!(1.5)), "float");
        assert_eq!(type_name(&json!([1])), "list");
        assert!(is_falsy(&json!(0)) && is_falsy(&json!("")) && !is_falsy(&json!("x")));
    }
}
