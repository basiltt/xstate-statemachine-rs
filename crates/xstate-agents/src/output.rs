//! Structured-output helpers shared by the agent logic and [`crate::structured`]: resolving
//! an output schema, parsing reply text and validating it.
//!
//! Python validates replies against a pydantic model; Rust validates against a JSON schema
//! with the structural checker of [`crate::tools::schema`]. Error details carry field names
//! and messages only, never the rejected input (it may quote the prompt): a `loc` segment
//! that is not a declared field name or an array index is rendered `*`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::{AgentError, Result};
use crate::pyfmt::{py_dumps, py_repr_str};
use crate::tools::schema::{SchemaError, field_names, validate};

/// The detail of a reply that is not JSON (passed through as-is from any parser).
pub const NOT_JSON: &str = "not valid JSON";

/// Turns reply text into the JSON value to validate. `Err` carries a short reason, never
/// reply content; [`NOT_JSON`] is reported as-is, anything else as `output rejected (..)`.
pub type OutputParser = Arc<dyn Fn(&str) -> std::result::Result<Value, String> + Send + Sync>;

/// Resolve an output-model spec: an inline JSON schema (object) or the name of a schema in
/// `named`.
///
/// # Errors
/// [`AgentError::Config`] for an unknown name or a spec of another type.
pub fn resolve_output_model(spec: &Value, named: &BTreeMap<String, Value>) -> Result<Value> {
    match spec {
        Value::Object(_) => Ok(spec.clone()),
        Value::String(name) => named.get(name).cloned().ok_or_else(|| {
            AgentError::config(format!(
                "output_model {} cannot be resolved: no schema of that name",
                py_repr_str(name)
            ))
        }),
        other => Err(AgentError::config(format!(
            "output_model must be a JSON schema or a schema name (got {other})"
        ))),
    }
}

/// Strict JSON, a surrounding Markdown code fence allowed (```` ```json ... ``` ````).
///
/// # Errors
/// `"not valid JSON"`.
pub fn parse_json_text(text: &str) -> std::result::Result<Value, String> {
    let mut t = text.trim();
    if t.starts_with("```") {
        t = t.trim_matches('`');
        if t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("json")) {
            t = &t[4..];
        }
    }
    serde_json::from_str(t).map_err(|_| NOT_JSON.to_owned())
}

/// The task a spawned agent starts on: `context["task"]` or `context["input"]["prompt"]`.
#[must_use]
pub fn task_of(ctx: &Value) -> Option<String> {
    let truthy = crate::pyfmt::truthy;
    let task = ctx
        .get("task")
        .filter(|t| truthy(t))
        .or_else(|| ctx.get("input").and_then(|i| i.get("prompt")));
    task.filter(|t| truthy(t)).map(crate::pyfmt::py_str)
}

/// `(ok, value_or_error_detail)` for one reply. No schema → the text itself is the value.
#[must_use]
pub fn validate_output(
    schema: Option<&Value>,
    text: &str,
    parser: Option<&OutputParser>,
) -> (bool, Value) {
    let Some(schema) = schema else { return (true, Value::String(text.to_owned())) };
    let parsed = match parser {
        Some(p) => p(text).map_err(|reason| {
            if reason == NOT_JSON { reason } else { format!("output rejected ({reason})") }
        }),
        None => parse_json_text(text),
    };
    let value = match parsed {
        Ok(v) => v,
        Err(detail) => return (false, Value::String(detail)),
    };
    match validate(schema, &value) {
        Ok(v) => (true, v),
        Err(errors) => (false, Value::String(validation_detail(schema, &errors))),
    }
}

/// Field paths and messages, never the reply: undeclared `loc` strings become `*`.
#[must_use]
pub fn validation_detail(schema: &Value, errors: &[SchemaError]) -> String {
    let allowed = field_names(schema);
    let parts: Vec<String> = errors
        .iter()
        .map(|e| {
            let loc: Vec<&str> = e
                .loc
                .iter()
                .zip(&e.index)
                .map(
                    |(p, is_index)| if *is_index || allowed.contains(p) { p.as_str() } else { "*" },
                )
                .collect();
            if loc.is_empty() { e.msg.clone() } else { format!("{}: {}", loc.join("."), e.msg) }
        })
        .collect();
    if parts.is_empty() { "did not validate".into() } else { parts.join("; ") }
}

/// The `RETRY_OUTPUT` user message: the validation detail and the schema to match.
#[must_use]
pub fn retry_prompt(schema: Option<&Value>, detail: &Value) -> Value {
    let schema_text = schema.map_or_else(|| "{}".to_owned(), py_dumps);
    let detail = crate::pyfmt::py_str(detail);
    json!({
        "role": "user",
        "content": format!("RETRY_OUTPUT: your answer did not validate ({detail}). Reply with only JSON matching this schema: {schema_text}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather() -> Value {
        json!({"type": "object", "properties": {"city": {"type": "string"}, "temp": {"type": "number"}}, "required": ["city", "temp"]})
    }

    #[test]
    fn replies_parse_and_validate() {
        let s = weather();
        assert_eq!(validate_output(None, "hi", None), (true, json!("hi")));
        let (ok, v) =
            validate_output(Some(&s), "```json\n{\"city\": \"Oslo\", \"temp\": 3}\n```", None);
        assert!(ok);
        assert_eq!(v, json!({"city": "Oslo", "temp": 3}));
        assert_eq!(validate_output(Some(&s), "nope", None), (false, json!("not valid JSON")));
        let (ok, detail) = validate_output(Some(&s), "{\"city\": 1, \"secret\": {\"k\": 1}}", None);
        assert!(!ok);
        assert_eq!(detail, json!("city: Input should be a valid string; temp: Field required"));
        let parser: OutputParser = Arc::new(|_| Err("KeyError".into()));
        assert_eq!(
            validate_output(Some(&s), "{}", Some(&parser)),
            (false, json!("output rejected (KeyError)"))
        );
    }

    #[test]
    fn undeclared_locations_are_masked() {
        let s = json!({"type": "object", "properties": {"m": {"type": "object", "additionalProperties": {"type": "integer"}}}});
        let (_, detail) = validate_output(Some(&s), "{\"m\": {\"<injected>\": \"x\"}}", None);
        assert_eq!(detail, json!("m.*: Input should be a valid integer"));
        assert_eq!(validation_detail(&s, &[]), "did not validate");
    }

    #[test]
    fn specs_tasks_and_prompts() {
        let mut named = BTreeMap::new();
        named.insert("W".to_owned(), weather());
        assert!(resolve_output_model(&json!("W"), &named).is_ok());
        assert!(resolve_output_model(&json!("X"), &named).is_err());
        assert!(resolve_output_model(&json!(1), &named).is_err());
        assert_eq!(task_of(&json!({"task": "t"})), Some("t".into()));
        assert_eq!(task_of(&json!({"task": "", "input": {"prompt": "p"}})), Some("p".into()));
        assert_eq!(task_of(&json!({})), None);
        let p = retry_prompt(Some(&json!({"type": "object"})), &json!("bad"));
        assert_eq!(
            p["content"],
            "RETRY_OUTPUT: your answer did not validate (bad). Reply with only JSON matching this schema: {\"type\": \"object\"}"
        );
        assert!(
            retry_prompt(None, &json!("x"))["content"]
                .as_str()
                .unwrap_or_default()
                .ends_with("schema: {}")
        );
    }
}
