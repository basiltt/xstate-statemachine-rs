//! The vendored AsyncAPI 3.0.0 schema and a structural validator.

use serde_json::Value;

use crate::error::{EdaError, Result};

/// The AsyncAPI 3.0.0 JSON Schema (`schemas/3.0.0.json` of `asyncapi/spec-json-schemas`,
/// bundled, as vendored by the Python library).
const SCHEMA: &str = include_str!("asyncapi_schema.json");

/// The vendored AsyncAPI 3.0.0 JSON Schema (offline).
///
/// # Errors
/// Never in practice: the embedded text is valid JSON (checked by the tests).
pub fn load_asyncapi_schema() -> Result<Value> {
    serde_json::from_str(SCHEMA)
        .map_err(|e| EdaError::InvalidArgument(format!("vendored schema: {e}")))
}

/// Resolve a local JSON pointer reference (`#/a/b`) inside `doc`.
fn resolve<'a>(doc: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    doc.pointer(pointer)
}

fn is_extension_key(key: &str) -> bool {
    key.strip_prefix("x-").is_some_and(|rest| {
        !rest.is_empty()
            && rest.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
    })
}

/// Validate `document` structurally against the vendored schema: the root's required and
/// allowed keys (taken from the schema), `asyncapi == "3.0.0"`, `info.title` / `info.version`,
/// channel addresses, every `$ref` under channels and operations resolving inside the document,
/// operation actions and operation messages belonging to their channel.
///
/// This is not a full JSON-Schema validator (no JSON-Schema crate is pulled in); it catches the
/// mistakes a generator or a hand edit makes.
///
/// # Errors
/// [`EdaError::InvalidAsyncApi`] listing every problem found.
pub fn validate_asyncapi(document: &Value) -> Result<()> {
    let schema = load_asyncapi_schema()?;
    let mut problems = Vec::new();
    let Some(root) = document.as_object() else {
        return Err(EdaError::InvalidAsyncApi(vec!["the document must be an object".into()]));
    };
    for required in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !root.contains_key(required) {
            problems.push(format!("missing required property {required:?}"));
        }
    }
    let allowed = schema["properties"].as_object();
    for k in root.keys() {
        if !allowed.is_some_and(|a| a.contains_key(k)) && !is_extension_key(k) {
            problems.push(format!("unexpected property {k:?}"));
        }
    }
    if root.get("asyncapi").and_then(Value::as_str) != Some(super::ASYNCAPI_VERSION) {
        problems.push(format!("asyncapi must be {:?}", super::ASYNCAPI_VERSION));
    }
    for field in ["title", "version"] {
        if !document["info"][field].is_string() {
            problems.push(format!("info.{field} must be a string"));
        }
    }
    check_channels(document, &mut problems);
    check_operations(document, &mut problems);
    if problems.is_empty() { Ok(()) } else { Err(EdaError::InvalidAsyncApi(problems)) }
}

fn check_ref(document: &Value, value: &Value, at: &str, problems: &mut Vec<String>) {
    match value.get("$ref").and_then(Value::as_str) {
        Some(r) if resolve(document, r).is_some() => {}
        Some(r) => problems.push(format!("{at}: unresolved $ref {r:?}")),
        None => problems.push(format!("{at}: expected a $ref object")),
    }
}

fn check_channels(document: &Value, problems: &mut Vec<String>) {
    let Some(channels) = document.get("channels") else { return };
    let Some(channels) = channels.as_object() else {
        problems.push("channels must be an object".into());
        return;
    };
    for (name, channel) in channels {
        if !channel.get("address").is_none_or(|a| a.is_string() || a.is_null()) {
            problems.push(format!("channels.{name}.address must be a string or null"));
        }
        for (k, m) in channel.get("messages").and_then(Value::as_object).into_iter().flatten() {
            check_ref(document, m, &format!("channels.{name}.messages.{k}"), problems);
        }
    }
}

fn check_operations(document: &Value, problems: &mut Vec<String>) {
    let Some(ops) = document.get("operations") else { return };
    let Some(ops) = ops.as_object() else {
        problems.push("operations must be an object".into());
        return;
    };
    for (name, op) in ops {
        let at = format!("operations.{name}");
        if !matches!(op.get("action").and_then(Value::as_str), Some("send" | "receive")) {
            problems.push(format!("{at}.action must be \"send\" or \"receive\""));
        }
        let channel = op.get("channel").unwrap_or(&Value::Null);
        check_ref(document, channel, &format!("{at}.channel"), problems);
        let channel_ref = channel.get("$ref").and_then(Value::as_str).unwrap_or_default();
        for (i, m) in op.get("messages").and_then(Value::as_array).into_iter().flatten().enumerate()
        {
            check_ref(document, m, &format!("{at}.messages[{i}]"), problems);
            let r = m.get("$ref").and_then(Value::as_str).unwrap_or_default();
            if !r.starts_with(&format!("{channel_ref}/messages/")) {
                problems.push(format!("{at}.messages[{i}] is not a message of {channel_ref:?}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_vendored() {
        let schema = load_asyncapi_schema().unwrap();
        assert_eq!(schema["$id"], "http://asyncapi.com/definitions/3.0.0/asyncapi.json");
        assert!(is_extension_key("x-foo") && !is_extension_key("x-") && !is_extension_key("foo"));
    }

    #[test]
    fn structural_problems_are_reported() {
        let bad = serde_json::json!({
            "asyncapi": "2.6.0",
            "info": {"title": "t"},
            "bogus": 1,
            "channels": {"c": {"address": 3, "messages": {"m": {"$ref": "#/components/messages/nope"}}}},
            "operations": {"o": {"action": "publish", "channel": {"$ref": "#/channels/c"},
                                  "messages": [{"$ref": "#/channels/d/messages/m"}]}}
        });
        let Err(EdaError::InvalidAsyncApi(problems)) = validate_asyncapi(&bad) else {
            panic!("valid?")
        };
        assert_eq!(problems.len(), 8, "{problems:#?}");
    }
}
