//! Redaction of sensitive keys (the Python `redact()` helper), shared by the inspector, the
//! dead-letter stores and the outbox.
//!
//! Keys are normalised to `snake_case` on both sides (camelCase boundaries split, `-` treated
//! as `_`, case-insensitive), so `apiKey`, `API_KEY` and `x-api-key` all match the denylist
//! entry `api_key`. A denylist entry of at most [`SHORT_KEY_MAX`] characters (`pin`, `pan`,
//! `otp`, `card`, `auth`, `seed`, ...) matches only a whole `_`-separated word of the key, so
//! `user_pin` redacts but `shipping`, `company`, `author` and `spinner` do not; longer entries
//! match as substrings (`session` redacts `sessionId`). Matching values are replaced by
//! `"***"`; objects and arrays are walked recursively; everything else is returned unchanged.
//! The input is never mutated.

use std::collections::BTreeSet;

use serde_json::Value;

/// Denylist entries of at most this many characters match only a whole word of the key.
pub const SHORT_KEY_MAX: usize = 4;

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
    "oauth",
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
    "creditcard",
    "cardnumber",
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

/// A compiled denylist: every entry normalised to lower `snake_case`, split into whole-word
/// (short) and substring (long) entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactKeys {
    short: BTreeSet<String>,
    long: Vec<String>,
}

impl Default for RedactKeys {
    fn default() -> Self {
        Self::new(DEFAULT_REDACT_KEYS.iter().copied())
    }
}

impl RedactKeys {
    /// Compile a denylist from key entries (short ones match whole words, long ones substrings).
    pub fn new<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (short, long): (Vec<String>, Vec<String>) = keys
            .into_iter()
            .map(|k| normalise(k.as_ref()))
            .partition(|k| k.chars().count() <= SHORT_KEY_MAX);
        Self { short: short.into_iter().collect(), long }
    }

    /// `true` when a whole word of `key` is a short entry, or `key` contains a long entry
    /// (after normalisation).
    #[must_use]
    pub fn matches(&self, key: &str) -> bool {
        if self.short.is_empty() && self.long.is_empty() {
            return false;
        }
        let key = normalise(key);
        key.split('_').any(|word| self.short.contains(word))
            || self.long.iter().any(|needle| key.contains(needle.as_str()))
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

/// Split camelCase boundaries (`[a-z0-9]` followed by `[A-Z]`), lower-case, map `-` to `_`.
fn normalise(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    let mut prev: Option<char> = None;
    for c in key.chars() {
        if c.is_ascii_uppercase()
            && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            out.push('_');
        }
        out.push(c);
        prev = Some(c);
    }
    out.to_lowercase().replace('-', "_")
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
    fn python_0120_denylist_entries_are_present() {
        let keys = RedactKeys::default();
        for key in ["oauth", "oauthToken", "creditcard", "cardnumber", "cardNumber"] {
            assert!(keys.matches(key), "{key} should match");
        }
    }

    #[test]
    fn short_entries_match_whole_words_only() {
        let keys = RedactKeys::default();
        for key in ["shipping", "company", "footprint", "discard", "author", "spinner"] {
            assert!(!keys.matches(key), "{key} must not be redacted");
        }
        for key in ["pin", "user_pin", "userPin", "cardNumber", "credit_card", "OTP", "auth"] {
            assert!(keys.matches(key), "{key} must be redacted");
        }
        // long entries keep substring matching
        assert!(keys.matches("sessionId"));
        assert!(keys.matches("x-api-key"));
        assert!(keys.matches("AUTHORIZATION"));
    }

    #[test]
    fn camel_boundaries_split_like_python() {
        // `[a-z0-9]` followed by `[A-Z]` splits; an acronym run does not.
        assert!(RedactKeys::default().matches("pinCode"));
        assert!(RedactKeys::default().matches("user2Pin"));
        assert!(!RedactKeys::default().matches("PINCODE"));
        // user entries are camel-split too: `apiKey` becomes `api_key`
        assert!(RedactKeys::new(["apiKey"]).matches("my-api-key"));
    }

    #[test]
    fn redacts_shipping_but_not_pin_in_a_document() {
        let v = json!({"shipping": "x", "company": "y", "pin": "1", "user_pin": "2"});
        assert_eq!(
            redact(&v),
            json!({"shipping": "x", "company": "y", "pin": "***", "user_pin": "***"})
        );
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
