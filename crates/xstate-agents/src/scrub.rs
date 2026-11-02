//! `scrub`: key-based redaction plus best-effort masking of secret-looking substrings.
//!
//! Key-based redaction ([`AGENT_REDACT_KEYS`], via [`xstate_core::redact::RedactKeys`])
//! cannot see a secret inside free text, so string values are also scanned for three value
//! patterns, matched exactly like the Python regular expression (case-insensitive):
//!
//! * `bearer\s+[A-Za-z0-9._~+/=-]{8,}` -- bearer tokens;
//! * `\b(sk|pk|rk|xox[abp])[-_][A-Za-z0-9_-]{8,}` -- `sk-` / `sk_` style API keys;
//! * `\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}` -- JWTs.
//!
//! Each match is replaced by `***`. Best-effort: it cannot recognise every secret format.

use serde_json::Value;
use xstate_core::redact::RedactKeys;

/// Keys redacted from tool results before they enter `context` (and so snapshots) and from
/// trace records. Substring, case-insensitive: `access_token`, `Authorization` and
/// `x-api-key` all match.
pub const AGENT_REDACT_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "token",
    "secret",
    "password",
    "bearer",
    "cookie",
    "private_key",
    "credential",
];

const MASK: &str = "***";
const MIN_RUN: usize = 8;

/// The compiled [`AGENT_REDACT_KEYS`].
#[must_use]
pub fn agent_redact_keys() -> RedactKeys {
    RedactKeys::new(AGENT_REDACT_KEYS.iter().copied())
}

/// Redact by key (unless `by_key` is false), then mask secret-looking substrings in every
/// string value. The input is not modified.
#[must_use]
pub fn scrub(value: &Value, by_key: bool) -> Value {
    let keyed = if by_key { agent_redact_keys().redact(value) } else { value.clone() };
    mask_values(keyed)
}

fn mask_values(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(mask_secrets(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(mask_values).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, mask_values(v))).collect())
        }
        other => other,
    }
}

/// Replace every secret-looking substring of `text` with `***`.
#[must_use]
pub fn mask_secrets(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let hit = bearer_at(&chars, i).or_else(|| key_at(&chars, i)).or_else(|| jwt_at(&chars, i));
        if let Some(end) = hit {
            out.push_str(MASK);
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn at_boundary(chars: &[char], i: usize) -> bool {
    i == 0 || !is_word(chars[i - 1])
}

/// Case-insensitive ASCII literal at `i`; returns the index after it.
fn literal(chars: &[char], i: usize, lit: &str) -> Option<usize> {
    let mut j = i;
    for want in lit.chars() {
        let c = chars.get(j)?;
        if !c.eq_ignore_ascii_case(&want) {
            return None;
        }
        j += 1;
    }
    Some(j)
}

/// Greedy run of `class` chars at `i` of at least [`MIN_RUN`]; returns its end.
fn run(chars: &[char], i: usize, class: fn(char) -> bool) -> Option<usize> {
    let len = chars[i.min(chars.len())..].iter().take_while(|c| class(**c)).count();
    (len >= MIN_RUN).then_some(i + len)
}

fn bearer_class(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._~+/=-".contains(c)
}

fn key_class(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn bearer_at(chars: &[char], i: usize) -> Option<usize> {
    let j = literal(chars, i, "bearer")?;
    let spaces = chars[j..].iter().take_while(|c| c.is_whitespace()).count();
    if spaces == 0 {
        return None;
    }
    run(chars, j + spaces, bearer_class)
}

fn key_at(chars: &[char], i: usize) -> Option<usize> {
    if !at_boundary(chars, i) {
        return None;
    }
    let after_prefix =
        ["sk", "pk", "rk"].iter().find_map(|p| literal(chars, i, p)).or_else(|| {
            let j = literal(chars, i, "xox")?;
            chars.get(j).filter(|c| "abp".contains(c.to_ascii_lowercase())).map(|_| j + 1)
        })?;
    let sep = chars.get(after_prefix)?;
    if *sep != '-' && *sep != '_' {
        return None;
    }
    run(chars, after_prefix + 1, key_class)
}

fn jwt_at(chars: &[char], i: usize) -> Option<usize> {
    if !at_boundary(chars, i) {
        return None;
    }
    let mut j = run(chars, literal(chars, i, "eyj")?, key_class)?;
    for _ in 0..2 {
        if chars.get(j) != Some(&'.') {
            return None;
        }
        j = run(chars, j + 1, key_class)?;
    }
    Some(j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_bearer_keys_and_jwts() {
        assert_eq!(mask_secrets("Bearer abcdefghijklmnop"), "***");
        assert_eq!(mask_secrets("auth: bearer   abc.def-ghi rest"), "auth: *** rest");
        assert_eq!(mask_secrets("bearer short"), "bearer short");
        assert_eq!(mask_secrets("key sk-abcdefgh12 end"), "key *** end");
        assert_eq!(mask_secrets("XOXB_abcdefghij"), "***");
        assert_eq!(mask_secrets("task-abcdefghijk"), "task-abcdefghijk");
        assert_eq!(mask_secrets("ask-abcdefghij"), "ask-abcdefghij");
        assert_eq!(mask_secrets("t eyJabcdefgh.abcdefgh.abcdefgh!"), "t ***!");
        assert_eq!(mask_secrets("eyJabcdefgh.abcdefgh"), "eyJabcdefgh.abcdefgh");
        assert_eq!(mask_secrets("héllo"), "héllo");
    }

    #[test]
    fn scrub_by_key_then_by_value() {
        let v =
            json!({"api_key": "k", "nested": [{"Authorization": "x"}, "sk_abcdefghij"], "n": 1});
        assert_eq!(
            scrub(&v, true),
            json!({"api_key": "***", "nested": [{"Authorization": "***"}, "***"], "n": 1})
        );
        let page = json!({"page_token": "abc"});
        assert_eq!(scrub(&page, false), page);
    }
}
