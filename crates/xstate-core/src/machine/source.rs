//! The configuration document a machine was compiled from (`Machine::to_json`).
//!
//! A machine built from a borrowed JSON value keeps the document as compact JSON text rather
//! than as a deep copy of the value: serialising writes one buffer, where cloning a
//! `serde_json::Value` allocates every map, key and string again. `serde_json` round-trips
//! values exactly (keys in order, integers as integers, floats with `float_roundtrip`), so the
//! document read back is equal to the input.

use std::cell::RefCell;

use serde_json::Value;

/// The source document of a machine.
#[derive(Debug)]
pub(crate) enum Source {
    /// The document as a value (machines built from a `MachineConfig`).
    Document(Value),
    /// The document as JSON text (machines built from JSON text or a borrowed value).
    Json(Box<str>),
}

impl Source {
    /// Keep `value` as compact JSON text.
    pub(crate) fn from_value(value: &Value) -> Self {
        thread_local! {
            static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        }
        let text = SCRATCH.with(|scratch| {
            let mut buf = scratch.borrow_mut();
            buf.clear();
            serde_json::to_writer(&mut *buf, value).ok()?;
            std::str::from_utf8(&buf).ok().map(Box::<str>::from)
        });
        // Writing a `Value` cannot fail (every key is a string); keep a copy if it ever did.
        text.map_or_else(|| Self::Document(value.clone()), Self::Json)
    }

    /// The document as a value.
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Document(value) => value.clone(),
            // The text was written by `serde_json` (or parsed by it), so it parses back.
            Self::Json(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_round_trips_the_value() {
        let value = serde_json::json!({
            "id": "m", "z": 1, "a": [1.5, -0.0, 1e300, 18_446_744_073_709_551_615_u64, -3],
            "s": "\u{1F600} \"q\" \n", "n": null, "o": {"y": true, "b": false}
        });
        let back = Source::from_value(&value).to_value();
        assert_eq!(back, value);
        let order: Vec<String> = match back {
            Value::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        };
        assert_eq!(order, ["id", "z", "a", "s", "n", "o"]);
    }
}
