//! Redaction of sensitive keys (the Python `redact()` helper), shared by the inspector, the
//! dead-letter stores and the outbox.
//!
//! Keys are matched by case-insensitive substring with `-` treated as `_` on both sides, so
//! `apiKey`, `API_KEY` and `x-api-key` all match the denylist entry `api_key`. Matching
//! values are replaced by `"***"`; objects and arrays are walked recursively; everything else
//! is returned unchanged. The input is never mutated.

use serde_json::Value;

/// Key substrings redacted by default: credentials, payment and banking identifiers, crypto
/// material and personal identifiers. Identical to the Python `DEFAULT_REDACT_KEYS`.
pub const DEFAULT_REDACT_KEYS: &[&str] = &[
    // credentials
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "auth",
    "bearer",
    "credential",
    "private_key",
    "cookie",
    "session",
    "signature",
    "otp",
    "pin",
    // payment / banking
    "card",
    "cvv",
    "cvc",
    "pan",
    "iban",
    "account_number",
    "account_no",
    "routing",
    "swift",
    // crypto
    "mnemonic",
    "seed_phrase",
    "seed",
    // personal identifiers
    "ssn",
    "dob",
    "date_of_birth",
    "email",
    "phone",
    "passport",
];

/// Placeholder written over redacted values.
pub const REDACTED: &str = "***";

/// A compiled denylist: every entry lower-cased with `-` normalised to `_`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactKeys {
    normalised: Vec<String>,
}

impl Default for RedactKeys {
    fn default() -> Self {
        Self::new(DEFAULT_REDACT_KEYS.iter().copied())
    }
}

impl RedactKeys {
    /// Compile a denylist from key substrings.
    pub fn new<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self { normalised: keys.into_iter().map(|k| normalise(k.as_ref())).collect() }
    }

    /// `true` when `key` contains any denylist entry (after normalisation).
    #[must_use]
    pub fn matches(&self, key: &str) -> bool {
        if self.normalised.is_empty() {
            return false;
        }
        let key = normalise(key);
        self.normalised.iter().any(|needle| key.contains(needle.as_str()))
    }

    /// Return `value` with every sensitive key replaced by [`REDACTED`].
    #[must_use]
    pub fn redact(&self, value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let out =
                            if self.matches(k) { Value::from(REDACTED) } else { self.redact(v) };
                        (k.clone(), out)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.redact(v)).collect()),
            other => other.clone(),
        }
    }

    /// Redact in place, avoiding a copy when the caller owns the value.
    pub fn redact_in_place(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if self.matches(k) {
                        *v = Value::from(REDACTED);
                    } else {
                        self.redact_in_place(v);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.redact_in_place(v)),
            _ => {}
        }
    }
}

fn normalise(key: &str) -> String {
    key.to_lowercase().replace('-', "_")
}

/// Redact `value` with the [`DEFAULT_REDACT_KEYS`].
#[must_use]
pub fn redact(value: &Value) -> Value {
    RedactKeys::default().redact(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_substrings_case_and_dash_insensitively() {
        let keys = RedactKeys::default();
        for key in ["apiKey", "API_KEY", "x-api-key", "Authorization", "user_email", "cardNumber"] {
            assert!(keys.matches(key), "{key} should match");
        }
        for key in ["name", "count", "user", "id"] {
            assert!(!keys.matches(key), "{key} should not match");
        }
    }

    #[test]
    fn user_denylist_spelled_with_dashes_matches() {
        let keys = RedactKeys::new(["X-Custom-Token"]);
        assert!(keys.matches("x_custom_token"));
        assert!(keys.matches("X-CUSTOM-TOKEN"));
        assert!(!keys.matches("password"));
    }

    #[test]
    fn redacts_recursively_without_mutating_input() {
        let input = json!({
            "user": {"name": "Ada", "email": "a@b", "items": [{"pan": "4111", "last4": "1111"}]},
            "list": [{"token": "t"}, 3, "s"],
            "password": {"nested": "gone"}
        });
        let before = input.clone();
        let out = redact(&input);
        assert_eq!(input, before);
        assert_eq!(
            out,
            json!({
                "user": {"name": "Ada", "email": "***", "items": [{"pan": "***", "last4": "1111"}]},
                "list": [{"token": "***"}, 3, "s"],
                "password": "***"
            })
        );
        let mut in_place = input;
        RedactKeys::default().redact_in_place(&mut in_place);
        assert_eq!(in_place, out);
    }

    #[test]
    fn empty_denylist_redacts_nothing() {
        let keys = RedactKeys::new(Vec::<&str>::new());
        let v = json!({"password": "p"});
        assert_eq!(keys.redact(&v), v);
    }
}
