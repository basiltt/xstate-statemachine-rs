//! [`ToolRegistry`]: name → [`Tool`], plus the enforcement `runTool` relies on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use super::{
    ALL_TOOLS, DEFAULT_MAX_OUTPUT_CHARS, DEFAULT_TOOL_TIMEOUT_S, Tool, ToolSpec, check_timeout,
};
use crate::error::{AgentError, Result};
use crate::messages::ToolCall;
use crate::pyfmt::{py_dumps, py_repr_str};

/// Name → [`Tool`], with output truncation and the X0.13 checks.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Tool>,
    max_output_chars: usize,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self { tools: BTreeMap::new(), max_output_chars: DEFAULT_MAX_OUTPUT_CHARS }
    }
}

/// Builder for a [`ToolRegistry`] (Python `tool_registry(*fns, timeout_s=, side_effect=,
/// max_output_chars=)`).
#[derive(Debug, Clone)]
pub struct ToolRegistryBuilder {
    specs: Vec<ToolSpec>,
    timeout_s: f64,
    side_effect: bool,
    max_output_chars: usize,
}

impl Default for ToolRegistryBuilder {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            timeout_s: DEFAULT_TOOL_TIMEOUT_S,
            side_effect: false,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
        }
    }
}

impl ToolRegistryBuilder {
    /// Add a tool.
    #[must_use]
    pub fn tool(mut self, spec: ToolSpec) -> Self {
        self.specs.push(spec);
        self
    }

    /// Default timeout for tools that set none (must be positive).
    #[must_use]
    pub const fn timeout_s(mut self, seconds: f64) -> Self {
        self.timeout_s = seconds;
        self
    }

    /// Default `side_effect` for every tool (a tool's own `true` always wins).
    #[must_use]
    pub const fn side_effect(mut self, flag: bool) -> Self {
        self.side_effect = flag;
        self
    }

    /// Output truncation bound in characters (>= 1).
    #[must_use]
    pub const fn max_output_chars(mut self, chars: usize) -> Self {
        self.max_output_chars = chars;
        self
    }

    /// Build the registry.
    ///
    /// # Errors
    /// [`AgentError::Config`]: duplicate names, a non-positive timeout, `"*"` as a name or
    /// `max_output_chars == 0`.
    pub fn build(self) -> Result<ToolRegistry> {
        check_timeout("<registry default>", self.timeout_s)?;
        if self.max_output_chars < 1 {
            return Err(AgentError::config("max_output_chars must be >= 1"));
        }
        let mut tools = BTreeMap::new();
        for spec in self.specs {
            let tool = spec.build(self.timeout_s, self.side_effect)?;
            if tools.contains_key(tool.name()) {
                return Err(AgentError::config(format!(
                    "duplicate tool name {}",
                    py_repr_str(tool.name())
                )));
            }
            tools.insert(tool.name().to_owned(), tool);
        }
        Ok(ToolRegistry { tools, max_output_chars: self.max_output_chars })
    }
}

/// A registry of `specs` with the default timeout and output bound.
///
/// # Errors
/// See [`ToolRegistryBuilder::build`].
pub fn tool_registry(specs: impl IntoIterator<Item = ToolSpec>) -> Result<ToolRegistry> {
    specs.into_iter().fold(ToolRegistry::builder(), ToolRegistryBuilder::tool).build()
}

impl ToolRegistry {
    /// Start a builder.
    #[must_use]
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    /// Registered names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Whether `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Number of tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// `true` when no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Look a tool up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    /// Output truncation bound.
    #[must_use]
    pub const fn max_output_chars(&self) -> usize {
        self.max_output_chars
    }

    /// The concrete tool names an allow-list admits (`"*"` → all registered).
    #[must_use]
    pub fn expand(&self, allowed: &[String]) -> BTreeSet<String> {
        if allowed.iter().any(|a| a == ALL_TOOLS) {
            return self.tools.keys().cloned().collect();
        }
        allowed.iter().filter(|n| self.tools.contains_key(*n)).cloned().collect()
    }

    /// Descriptors of the tools `allowed` admits: what the model is told exists. A tool it
    /// is never told about can still be NAMED by an injected instruction;
    /// [`authorise`](Self::authorise) is what refuses it.
    #[must_use]
    pub fn schemas(&self, allowed: &[String]) -> Vec<Value> {
        self.expand(allowed).iter().filter_map(|n| self.tools.get(n)).map(Tool::schema).collect()
    }

    /// Why `call` may not run, or `None`. Used by the guards AND by `authorise` so the two
    /// cannot disagree.
    #[must_use]
    pub fn denial(
        &self,
        call: &ToolCall,
        allowed: &[String],
        approved_ids: &[String],
    ) -> Option<&'static str> {
        let Some(tool) = self.tools.get(&call.name) else { return Some("not registered") };
        if !self.expand(allowed).contains(&call.name) {
            return Some("not in this state's meta.tools allow-list");
        }
        if tool.is_side_effect() && !approved_ids.contains(&call.id) {
            return Some("side_effect tool without human approval");
        }
        None
    }

    /// Run every X0.13 check; return the validated arguments. Nothing is executed here.
    ///
    /// # Errors
    /// [`AgentError::ToolDenied`] with the reason.
    pub fn authorise(
        &self,
        call: &ToolCall,
        allowed: &[String],
        approved_ids: &[String],
    ) -> Result<Value> {
        if let Some(reason) = self.denial(call, allowed, approved_ids) {
            return Err(AgentError::denied(&call.name, reason));
        }
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| AgentError::denied(&call.name, "not registered"))?;
        tool.validate(&call.arguments)
    }

    /// Render a result as text (a string as-is, anything else as JSON) cut to
    /// `max_output_chars` characters with a truncation marker.
    #[must_use]
    pub fn truncate(&self, value: &Value) -> String {
        let text = match value {
            Value::String(s) => s.clone(),
            other => py_dumps(other),
        };
        let len = text.chars().count();
        let limit = self.max_output_chars;
        if len <= limit {
            return text;
        }
        let head: String = text.chars().take(limit).collect();
        format!("{head}…[truncated {} chars]", len - limit)
    }

    /// Execute on a worker thread bounded by `timeout_s`.
    ///
    /// A tool that overruns keeps running in the background (a thread cannot be killed)
    /// while the machine moves on; tools with external effects should honour their own
    /// timeouts too.
    ///
    /// # Errors
    /// [`AgentError::ToolTimeout`], the tool's own error, or [`AgentError::Tool`] if it
    /// panicked or is unknown.
    pub fn call(&self, call: &ToolCall, args: Value) -> Result<Value> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| AgentError::denied(&call.name, "not registered"))?;
        let func = tool.func();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("xsm-tool-{}", tool.name()))
            .spawn(move || {
                let _ = tx.send(func(args));
            })
            .map_err(|e| AgentError::tool(format!("cannot start tool thread: {e}")))?;
        match rx.recv_timeout(Duration::from_secs_f64(tool.timeout_s())) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(AgentError::ToolTimeout {
                tool: tool.name().to_owned(),
                timeout_s: tool.timeout_s(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(AgentError::tool(format!("tool {} panicked", py_repr_str(tool.name()))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> ToolRegistry {
        ToolRegistry::builder()
            .tool(ToolSpec::new("echo", Ok))
            .tool(ToolSpec::new("pay", |_| Ok(json!("paid"))).side_effect(true))
            .tool(
                ToolSpec::new("slow", |_| {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(Value::Null)
                })
                .timeout_s(0.05),
            )
            .tool(ToolSpec::new("boom", |_| panic!("boom")))
            .max_output_chars(10)
            .build()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn all() -> Vec<String> {
        vec![ALL_TOOLS.to_owned()]
    }

    #[test]
    fn registry_builds_and_expands() {
        let r = registry();
        assert_eq!(r.names(), ["boom", "echo", "pay", "slow"]);
        assert_eq!(r.len(), 4);
        assert!(!r.is_empty() && r.contains("echo"));
        assert_eq!(r.expand(&["echo".into(), "nope".into()]).len(), 1);
        assert_eq!(r.schemas(&all()).len(), 4);
        assert_eq!(r.schemas(&[]), Vec::<Value>::new());
        assert!(tool_registry([ToolSpec::new("a", Ok), ToolSpec::new("a", Ok)]).is_err());
        assert!(ToolRegistry::builder().max_output_chars(0).build().is_err());
        assert!(ToolRegistry::builder().timeout_s(-1.0).build().is_err());
        assert_eq!(ToolRegistry::default().max_output_chars(), DEFAULT_MAX_OUTPUT_CHARS);
    }

    #[test]
    fn authorise_runs_every_check() {
        let r = registry();
        let call = |name: &str| ToolCall::new("c1", name, json!({}));
        let deny = |res: Result<Value>| res.err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(
            deny(r.authorise(&call("nope"), &all(), &[])),
            "tool 'nope' denied: not registered"
        );
        assert_eq!(
            deny(r.authorise(&call("echo"), &[], &[])),
            "tool 'echo' denied: not in this state's meta.tools allow-list"
        );
        assert_eq!(
            deny(r.authorise(&call("pay"), &all(), &[])),
            "tool 'pay' denied: side_effect tool without human approval"
        );
        assert!(r.authorise(&call("pay"), &all(), &["c1".into()]).is_ok());
    }

    #[test]
    fn calls_are_bounded_and_truncated() {
        let r = registry();
        assert_eq!(
            r.call(&ToolCall::new("1", "pay", json!({})), json!({}))
                .unwrap_or_else(|e| panic!("{e}")),
            json!("paid")
        );
        let slow = r.call(&ToolCall::new("1", "slow", json!({})), json!({}));
        assert_eq!(
            slow.err().map(|e| e.to_string()),
            Some("tool 'slow' exceeded timeout_s=0.05".into())
        );
        assert!(r.call(&ToolCall::new("1", "boom", json!({})), json!({})).is_err());
        assert!(r.call(&ToolCall::new("1", "nope", json!({})), json!({})).is_err());
        assert_eq!(r.truncate(&json!("short")), "short");
        assert_eq!(r.truncate(&json!("0123456789abc")), "0123456789…[truncated 3 chars]");
        assert_eq!(r.truncate(&json!({"a": 1})), "{\"a\": 1}");
    }
}
