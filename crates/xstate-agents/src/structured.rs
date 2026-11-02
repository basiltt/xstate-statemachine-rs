//! Structured output per state: the public face of `RETRY_OUTPUT`.
//!
//! The mechanism lives in the agent logic: the `outputValid` guard validates the final reply
//! against the agent's output schema or the hosting state's `meta.output_model`,
//! `retryOutput` re-prompts with the validation errors (field names, never values), and
//! `canRetryOutput` sends exhaustion to `error`. This module is the thin wrapper:
//!
//! * [`structured_output`] → the settings that switch it on ([`AgentLogic::structured`]);
//! * [`validate_structured`] validates one value the same way, for use outside a chart.
//!
//! Python can parse replies with instructor's lenient extractor when installed; Rust always
//! uses the strict parser ([`json_parser`]: JSON, a Markdown code fence allowed) unless a
//! custom [`OutputParser`] is supplied.

use std::sync::Arc;

use serde_json::Value;

use crate::error::{AgentError, Result};
use crate::logic::AgentLogic;
use crate::output::{OutputParser, parse_json_text, validate_output};
use crate::pyfmt::py_dumps;

/// Settings for validated structured output.
#[derive(Clone)]
pub struct StructuredOutput {
    /// Schema every final reply must match; `None` → each state's `meta.output_model`.
    pub output_model: Option<Value>,
    /// `RETRY_OUTPUT` re-prompts before `error` (`kind: "output"`); each is a model turn.
    pub max_output_retries: u32,
    /// Reply-text → JSON parser.
    pub output_parser: OutputParser,
}

impl std::fmt::Debug for StructuredOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructuredOutput")
            .field("output_model", &self.output_model)
            .field("max_output_retries", &self.max_output_retries)
            .finish_non_exhaustive()
    }
}

/// The strict reply parser: JSON, a surrounding code fence allowed.
#[must_use]
pub fn json_parser() -> OutputParser {
    Arc::new(parse_json_text)
}

/// Settings for structured output with `retries` re-prompts.
///
/// # Errors
/// [`AgentError::Config`] when `schema` is given and is not a JSON object.
pub fn structured_output(schema: Option<Value>, retries: u32) -> Result<StructuredOutput> {
    if let Some(s) = &schema
        && !s.is_object()
    {
        return Err(AgentError::config(format!(
            "output_model must be a JSON schema object (got {s})"
        )));
    }
    Ok(StructuredOutput {
        output_model: schema,
        max_output_retries: retries,
        output_parser: json_parser(),
    })
}

impl AgentLogic {
    /// Apply [`StructuredOutput`] settings.
    #[must_use]
    pub fn structured(self, settings: StructuredOutput) -> Self {
        self.output_model(settings.output_model)
            .max_output_retries(settings.max_output_retries)
            .output_parser(settings.output_parser)
    }
}

/// `(ok, value_or_error_detail)` for one reply: `value` is reply text (a JSON string) or an
/// already-parsed value.
///
/// # Errors
/// [`AgentError::Config`] when `schema` is not a JSON object.
pub fn validate_structured(schema: &Value, value: &Value) -> Result<(bool, Value)> {
    if !schema.is_object() {
        return Err(AgentError::config("output_model must be a JSON schema object"));
    }
    let text = match value {
        Value::String(s) => s.clone(),
        other => py_dumps(other),
    };
    Ok(validate_output(Some(schema), &text, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_settings_and_validation() {
        assert!(structured_output(Some(json!("x")), 1).is_err());
        let s =
            structured_output(Some(json!({"type": "object"})), 3).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.max_output_retries, 3);
        assert!(format!("{s:?}").contains("StructuredOutput"));
        let schema =
            json!({"type": "object", "properties": {"a": {"type": "integer"}}, "required": ["a"]});
        assert_eq!(
            validate_structured(&schema, &json!({"a": 1})).unwrap_or_else(|e| panic!("{e}")),
            (true, json!({"a": 1}))
        );
        assert_eq!(
            validate_structured(&schema, &json!("{\"a\": \"x\"}"))
                .unwrap_or_else(|e| panic!("{e}")),
            (false, json!("a: Input should be a valid integer"))
        );
        assert!(validate_structured(&json!(1), &json!(1)).is_err());
        assert_eq!(json_parser()("[1]"), Ok(json!([1])));
    }
}
