//! Python-compatible JSON and `repr` rendering, used to reproduce the Python library's
//! `structure_hash` and inspector recordings byte-for-byte (ADR-0003).
//!
//! [`dumps`] mirrors `json.dumps(value, sort_keys=True)` with the default separators and
//! `ensure_ascii=True`; [`dumps_compact`] mirrors `json.dumps(value, separators=(",", ":"))`
//! in insertion order; `repr_str` mirrors `repr()` of a `str`.

use std::fmt::Write as _;

use serde_json::Value;

/// Separators and key order: Python's defaults or the compact form.
#[derive(Debug, Clone, Copy)]
struct Style {
    sort_keys: bool,
    item_sep: &'static str,
    key_sep: &'static str,
}

/// `json.dumps(value, sort_keys=True)` (default separators, `ensure_ascii=True`).
#[must_use]
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out, Style { sort_keys: true, item_sep: ", ", key_sep: ": " });
    out
}

/// `json.dumps(value, separators=(",", ":"), ensure_ascii=True)` in insertion order.
#[must_use]
pub fn dumps_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out, Style { sort_keys: false, item_sep: ",", key_sep: ":" });
    out
}

fn write_value(value: &Value, out: &mut String, style: Style) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else if let Some(f) = n.as_f64() {
                out.push_str(&py_float_repr(f));
            }
        }
        Value::String(s) => write_json_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_sep);
                }
                write_value(item, out, style);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            if style.sort_keys {
                keys.sort();
            }
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_sep);
                }
                write_json_str(key, out);
                out.push_str(style.key_sep);
                write_value(&map[key], out, style);
            }
            out.push('}');
        }
    }
}

/// JSON string literal with `ensure_ascii=True` escaping.
fn write_json_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python `repr(float)`: shortest round-trip representation, always with a fraction or an
/// exponent, `inf`/`nan` spelled like Python.
fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') { s } else { format!("{s}.0") }
}

/// Python `repr(str)`: single quotes unless the string contains `'` and no `"`; backslash
/// escapes for `\\`, the chosen quote and control characters; other printable characters
/// (including non-ASCII) are kept as-is.
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

/// Python `repr(tuple_of_str)` for a tuple of already-rendered elements (`('a',)`, `()`).
pub(crate) fn repr_tuple(items: &[String]) -> String {
    match items {
        [] => "()".to_owned(),
        [only] => format!("({only},)"),
        many => format!("({})", many.join(", ")),
    }
}

/// Python `repr(None)` / `repr(bool)`.
pub(crate) const NONE: &str = "None";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_python_defaults() {
        let v = serde_json::json!({"b": [1, "x"], "a": null, "c": {"z": true, "y": "é\""}});
        assert_eq!(dumps(&v), r#"{"a": null, "b": [1, "x"], "c": {"y": "\u00e9\"", "z": true}}"#);
    }

    #[test]
    fn dumps_compact_keeps_insertion_order() {
        let v = serde_json::json!({"b": [1, "x"], "a": {"z": true, "y": "\u{7f}"}});
        assert_eq!(dumps_compact(&v), r#"{"b":[1,"x"],"a":{"z":true,"y":"\u007f"}}"#);
    }

    #[test]
    fn repr_str_quotes_like_python() {
        assert_eq!(repr_str("abc"), "'abc'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("both'\""), "'both\\'\"'");
        assert_eq!(repr_str("a\\b\n"), "'a\\\\b\\n'");
        assert_eq!(repr_tuple(&["'a'".into()]), "('a',)");
        assert_eq!(repr_tuple(&[]), "()");
    }
}
