//! Tools: [`ToolSpec`] → [`Tool`] in a [`ToolRegistry`], enforced (X0.13).
//!
//! The model PROPOSES a tool call; this module DECIDES. Every check the chart's
//! `toolAllowed` / `needsHuman` guards make is made AGAIN inside
//! [`ToolRegistry::authorise`], which `runTool` calls before executing anything -- so a chart
//! that drops a guard, a restored snapshot carrying a forged `pending_tool_calls`, or a
//! prompt-injected tool result cannot get a tool executed that the active state does not
//! allow:
//!
//! 1. the tool is REGISTERED;
//! 2. it is in the active state's `meta.tools` allow-list (`"*"` = every registered tool);
//! 3. its arguments validate against its schema (unknown keys are refused, not dropped);
//! 4. `side_effect` tools run only for call ids a human approved;
//! 5. every tool has a mandatory `timeout_s`;
//! 6. the output is truncated to `max_output_chars` before it re-enters the conversation.
//!
//! A refusal is an [`AgentError::ToolDenied`] raised BEFORE the function is called.

pub mod approval;
mod registry;
pub mod schema;

use std::fmt;
use std::sync::Arc;

use serde_json::{Value, json};

pub use registry::{ToolRegistry, ToolRegistryBuilder, tool_registry};

use crate::error::{AgentError, Result};

/// Default per-tool timeout when neither the tool nor the registry sets one.
pub const DEFAULT_TOOL_TIMEOUT_S: f64 = 30.0;
/// Tool output longer than this (in characters) is truncated before the model sees it.
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 4000;
/// The allow-list wildcard: every REGISTERED tool.
pub const ALL_TOOLS: &str = "*";

/// A tool implementation: validated arguments in, a JSON result out. It runs on a worker
/// thread bounded by the tool's `timeout_s`.
pub type ToolFn = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;

/// The schema used when a tool declares none: an object with no parameters.
#[must_use]
pub fn empty_parameters() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

/// Description of one tool before it joins a registry (Python `tool(fn, ...)`).
#[derive(Clone)]
pub struct ToolSpec {
    name: String,
    description: String,
    parameters: Value,
    timeout_s: Option<f64>,
    side_effect: bool,
    func: ToolFn,
}

impl fmt::Debug for ToolSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolSpec")
            .field("name", &self.name)
            .field("timeout_s", &self.timeout_s)
            .field("side_effect", &self.side_effect)
            .finish_non_exhaustive()
    }
}

impl ToolSpec {
    /// A tool named `name` running `func`. Parameters default to "no arguments", the timeout
    /// to the registry's default.
    pub fn new(
        name: impl Into<String>,
        func: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            parameters: empty_parameters(),
            timeout_s: None,
            side_effect: false,
            func: Arc::new(func),
        }
    }

    /// Description sent to the model.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = text.into();
        self
    }

    /// JSON schema of the arguments (an `object` schema; use `additionalProperties: false`
    /// to refuse unknown keys).
    #[must_use]
    pub fn parameters(mut self, schema: Value) -> Self {
        self.parameters = schema;
        self
    }

    /// Execution bound in seconds (must be positive).
    #[must_use]
    pub const fn timeout_s(mut self, seconds: f64) -> Self {
        self.timeout_s = Some(seconds);
        self
    }

    /// `true` → each call needs human approval (`HUMAN_APPROVED`).
    #[must_use]
    pub const fn side_effect(mut self, flag: bool) -> Self {
        self.side_effect = flag;
        self
    }

    /// Resolve registry defaults into a [`Tool`].
    ///
    /// # Errors
    /// [`AgentError::Config`] for a non-positive timeout or a reserved name.
    pub fn build(self, default_timeout_s: f64, default_side_effect: bool) -> Result<Tool> {
        let timeout = self.timeout_s.unwrap_or(default_timeout_s);
        check_timeout(&self.name, timeout)?;
        if self.name == ALL_TOOLS {
            return Err(AgentError::config("'*' is reserved for the wildcard"));
        }
        Ok(Tool {
            name: self.name,
            description: self.description,
            parameters: self.parameters,
            timeout_s: timeout,
            side_effect: self.side_effect || default_side_effect,
            func: self.func,
        })
    }
}

pub(crate) fn check_timeout(name: &str, timeout: f64) -> Result<()> {
    if timeout.is_finite() && timeout > 0.0 {
        return Ok(());
    }
    Err(AgentError::config(format!(
        "tool {}: timeout_s must be a positive number (got {}); every tool is bounded (X0.13)",
        crate::pyfmt::py_repr_str(name),
        crate::pyfmt::py_float(timeout)
    )))
}

/// One registered tool.
#[derive(Clone)]
pub struct Tool {
    name: String,
    description: String,
    parameters: Value,
    timeout_s: f64,
    side_effect: bool,
    func: ToolFn,
}

impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("timeout_s", &self.timeout_s)
            .field("side_effect", &self.side_effect)
            .finish_non_exhaustive()
    }
}

impl Tool {
    /// The name the model calls it by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Description sent to the model.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// JSON schema of the arguments.
    #[must_use]
    pub const fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Mandatory execution bound in seconds.
    #[must_use]
    pub const fn timeout_s(&self) -> f64 {
        self.timeout_s
    }

    /// Whether each call needs human approval.
    #[must_use]
    pub const fn is_side_effect(&self) -> bool {
        self.side_effect
    }

    pub(crate) fn func(&self) -> ToolFn {
        Arc::clone(&self.func)
    }

    /// The provider-neutral descriptor `{name, description, parameters}`.
    #[must_use]
    pub fn schema(&self) -> Value {
        json!({"name": self.name, "description": self.description, "parameters": self.parameters})
    }

    /// Validate model-proposed `arguments`; return them with defaults filled in.
    ///
    /// # Errors
    /// [`AgentError::ToolDenied`] naming the failing fields (never their values).
    pub fn validate(&self, arguments: &Value) -> Result<Value> {
        if !arguments.is_object() {
            return Err(AgentError::denied(&self.name, "arguments must be an object"));
        }
        schema::validate(&self.parameters, arguments).map_err(|errors| {
            if errors.iter().any(|e| e.msg == "nested too deeply") {
                return AgentError::denied(&self.name, "invalid arguments (nested too deeply)");
            }
            AgentError::denied(
                &self.name,
                format!("invalid arguments ({})", schema::render_errors(&errors)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_resolve_defaults() {
        let t = ToolSpec::new("t", |_| Ok(Value::Null))
            .description("d")
            .build(5.0, true)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!((t.timeout_s() - 5.0).abs() < f64::EPSILON);
        assert!(t.is_side_effect());
        assert_eq!(t.schema()["description"], "d");
        assert!(ToolSpec::new("t", |_| Ok(Value::Null)).timeout_s(0.0).build(1.0, false).is_err());
        assert!(ToolSpec::new("*", |_| Ok(Value::Null)).build(1.0, false).is_err());
        assert!(format!("{t:?}").contains("Tool"));
    }

    #[test]
    fn validation_denies_with_field_names() {
        let t = ToolSpec::new("t", |_| Ok(Value::Null))
            .build(1.0, false)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.validate(&json!({})).unwrap_or_else(|e| panic!("{e}")), json!({}));
        assert_eq!(
            t.validate(&json!([1])).err().map(|e| e.to_string()),
            Some("tool 't' denied: arguments must be an object".into())
        );
        assert_eq!(
            t.validate(&json!({"x": 1})).err().map(|e| e.to_string()),
            Some("tool 't' denied: invalid arguments (x: Extra inputs are not permitted)".into())
        );
    }
}
