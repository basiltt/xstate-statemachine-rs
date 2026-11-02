//! `json.dumps(value, indent=2)` as the Python library writes snapshots: insertion-ordered
//! keys, `", "`-free separators (`","` + newline and `": "`), `ensure_ascii=True` escaping,
//! `[]` / `{}` for empty containers and Python `repr(float)` number formatting. Matching it
//! byte for byte lets a Rust-written snapshot be diffed, hashed or deduplicated against a
//! Python-written one.

use std::fmt::Write as _;

use serde_json::{Number, Value};

/// Render `value` exactly like Python's `json.dumps(value, indent=2)`.
pub(crate) fn dumps_indent(value: &Value) -> String {
    let mut out = String::with_capacity(1024);
    write_value(value, 0, &mut out);
    out
}

fn write_value(value: &Value, level: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => write_str(s, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(level + 1, out);
                write_value(item, level + 1, out);
            }
            newline(level, out);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(level + 1, out);
                write_str(key, out);
                out.push_str(": ");
                write_value(item, level + 1, out);
            }
            newline(level, out);
            out.push('}');
        }
    }
}

fn newline(level: usize, out: &mut String) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_number(n: &Number, out: &mut String) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else if let Some(f) = n.as_f64() {
        write_float(f, out);
    }
}

/// Python `repr(float)`: the shortest round-trip digits, fixed notation when the decimal
/// exponent is in `-4..16`, otherwise `d.ddde+XX` with at least two exponent digits.
pub(crate) fn write_float(f: f64, out: &mut String) {
    if f.is_nan() {
        out.push_str("NaN");
        return;
    }
    if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    let sci = format!("{f:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..16).contains(&exponent) {
        let fixed = format!("{f}");
        out.push_str(&fixed);
        if !fixed.contains('.') {
            out.push_str(".0");
        }
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        let _ = write!(out, "{mantissa}e{sign}{:02}", exponent.unsigned_abs());
    }
}

/// JSON string literal with Python's `ensure_ascii=True` escaping.
fn write_str(s: &str, out: &mut String) {
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
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}
