//! Python-compatible text rendering: `repr()` of a `str` and a `float`, and
//! `json.dumps(value, ensure_ascii=False)` (insertion order, default separators), so text
//! that enters the conversation (tool output, retry prompts, error messages) is
//! byte-identical to the Python library's.

use std::fmt::Write as _;

use serde_json::Value;

/// `repr(s)` for a Python `str`: single quotes unless the text holds a `'` and no `"`.
pub(crate) fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
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

/// `repr(f)` for a finite Python `float`: `5.0`, `0.25`, `1e+16`, `1e-05`.
pub(crate) fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        return exponent_form(f);
    }
    let text = format!("{f}");
    if text.contains('.') { text } else { format!("{text}.0") }
}

fn exponent_form(f: f64) -> String {
    let text = format!("{f:e}");
    let (mantissa, exp) = text.split_once('e').unwrap_or((text.as_str(), "0"));
    let (sign, digits) = exp.strip_prefix('-').map_or(("+", exp), |d| ("-", d));
    format!("{mantissa}e{sign}{digits:0>2}")
}

/// `json.dumps(value, ensure_ascii=False)`: insertion order, `", "` / `": "` separators.
pub(crate) fn py_dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&py_float(n.as_f64().unwrap_or(0.0)));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(k, out);
                out.push_str(": ");
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

fn write_str(s: &str, out: &mut String) {
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
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `str(value)` as Python would render a JSON value that is used as text: a string is
/// itself, anything else its JSON text.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => py_dumps(other),
    }
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_render_like_python() {
        assert_eq!(py_float(5.0), "5.0");
        assert_eq!(py_float(0.25), "0.25");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(py_float(-2.5e20), "-2.5e+20");
        assert_eq!(py_float(f64::NAN), "nan");
        assert_eq!(py_float(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn dumps_matches_json_dumps() {
        let v = json!({"b": 1, "a": [true, null, 1.5, "é\n\"x\"\u{1}"], "c": {}});
        assert_eq!(
            py_dumps(&v),
            "{\"b\": 1, \"a\": [true, null, 1.5, \"é\\n\\\"x\\\"\\u0001\"], \"c\": {}}"
        );
    }

    #[test]
    fn repr_and_truthiness() {
        assert_eq!(py_repr_str("refund"), "'refund'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(py_repr_str("a'\"\\\t\u{1}"), "'a\\'\"\\\\\\t\\x01'");
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!([0])));
        assert!(!truthy(&json!(0)));
        assert_eq!(py_str(&json!(3)), "3");
        assert_eq!(py_str(&json!("x")), "x");
    }
}
