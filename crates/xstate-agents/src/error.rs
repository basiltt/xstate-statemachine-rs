//! The one error type of the crate: the Python `AgentError` hierarchy as an enum.
//!
//! | Python                      | Rust                               |
//! |-----------------------------|------------------------------------|
//! | `AgentError`                | [`AgentError::Agent`]              |
//! | `ToolDeniedError`           | [`AgentError::ToolDenied`]         |
//! | `ToolTimeoutError`          | [`AgentError::ToolTimeout`]        |
//! | `AgentConfigError`          | [`AgentError::Config`]             |
//! | any exception a tool raises | [`AgentError::Tool`]               |
//!
//! Inside the chart a service failure travels as the text of an `xstate.error.actor.*` event.
//! [`AgentError::to_event_error`] renders it as `"<ClassName>: <message>"` so the
//! `isToolDenied` guard and the `recordFailure` action can recover the class without trusting
//! free text from a tool (a tool failure is always rendered as `ToolError`).

use crate::pyfmt::{py_float, py_repr_str};

/// Everything that can go wrong in an agent.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// Generic agent failure (Python `AgentError`), e.g. a [`FakeModel`](crate::FakeModel)
    /// script that ran out.
    #[error("{0}")]
    Agent(String),
    /// A tool call was refused and NOT executed (X0.13).
    #[error("tool {} denied: {reason}", py_repr_str(.tool))]
    ToolDenied {
        /// The tool the model named.
        tool: String,
        /// Why it was refused.
        reason: String,
    },
    /// A tool exceeded its mandatory `timeout_s`.
    #[error("tool {} exceeded timeout_s={}", py_repr_str(.tool), py_float(*.timeout_s))]
    ToolTimeout {
        /// The tool.
        tool: String,
        /// Its bound in seconds.
        timeout_s: f64,
    },
    /// The agent wiring is invalid; always raised loudly, never silently narrowed.
    #[error("{0}")]
    Config(String),
    /// A tool implementation failed.
    #[error("{0}")]
    Tool(String),
    /// The state machine engine failed.
    #[error("engine: {0}")]
    Engine(String),
}

/// Result alias of the crate.
pub type Result<T, E = AgentError> = std::result::Result<T, E>;

/// Class names an error event may carry (see [`AgentError::class_name`]).
const CLASSES: &[&str] =
    &["AgentError", "ToolDeniedError", "ToolTimeoutError", "AgentConfigError", "ToolError"];

impl AgentError {
    /// Shorthand for [`AgentError::Config`].
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Shorthand for [`AgentError::Tool`]: what a tool implementation returns on failure.
    pub fn tool(message: impl Into<String>) -> Self {
        Self::Tool(message.into())
    }

    /// Shorthand for [`AgentError::ToolDenied`].
    pub fn denied(tool: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ToolDenied { tool: tool.into(), reason: reason.into() }
    }

    /// The Python class name this variant stands for (`ToolDeniedError`, ...). Engine
    /// failures report `AgentError`.
    #[must_use]
    pub const fn class_name(&self) -> &'static str {
        match self {
            Self::Agent(_) | Self::Engine(_) => "AgentError",
            Self::ToolDenied { .. } => "ToolDeniedError",
            Self::ToolTimeout { .. } => "ToolTimeoutError",
            Self::Config(_) => "AgentConfigError",
            Self::Tool(_) => "ToolError",
        }
    }

    /// `true` for [`AgentError::ToolDenied`].
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        matches!(self, Self::ToolDenied { .. })
    }

    /// The `"<ClassName>: <message>"` text carried by an error event.
    #[must_use]
    pub fn to_event_error(&self) -> String {
        format!("{}: {self}", self.class_name())
    }

    /// Wrap into an engine error for a service / action return value.
    #[must_use]
    pub fn into_engine(self) -> xstate_core::Error {
        xstate_core::Error::logic(self.to_event_error())
    }
}

impl From<xstate_core::Error> for AgentError {
    fn from(err: xstate_core::Error) -> Self {
        Self::Engine(err.to_string())
    }
}

/// Map an agent result into the engine's result type (for action / service bodies).
pub(crate) fn engine_err<T>(r: Result<T>) -> xstate_core::Result<T> {
    r.map_err(AgentError::into_engine)
}

/// Split an error-event text into `(class name, message)`. Text that does not start with a
/// known class (an engine error) is classed `AgentError`.
#[must_use]
pub fn split_event_error(text: &str) -> (&'static str, &str) {
    for class in CLASSES {
        if let Some(rest) = text.strip_prefix(class).and_then(|r| r.strip_prefix(": ")) {
            return (class, rest);
        }
    }
    ("AgentError", text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_python() {
        let e = AgentError::denied("refund", "side_effect tool without human approval");
        assert_eq!(e.to_string(), "tool 'refund' denied: side_effect tool without human approval");
        let t = AgentError::ToolTimeout { tool: "slow".into(), timeout_s: 5.0 };
        assert_eq!(t.to_string(), "tool 'slow' exceeded timeout_s=5.0");
        assert_eq!(t.class_name(), "ToolTimeoutError");
    }

    #[test]
    fn event_errors_round_trip() {
        let e = AgentError::denied("x", "not registered");
        let text = e.to_event_error();
        assert_eq!(
            split_event_error(&text),
            ("ToolDeniedError", "tool 'x' denied: not registered")
        );
        assert_eq!(split_event_error("boom"), ("AgentError", "boom"));
        let engine: AgentError = xstate_core::Error::logic("x").into();
        assert_eq!(engine.class_name(), "AgentError");
        assert!(AgentError::denied("a", "b").is_denied());
        assert!(!AgentError::tool("a").is_denied());
        assert_eq!(AgentError::config("c").class_name(), "AgentConfigError");
        assert!(AgentError::tool("t").into_engine().to_string().starts_with("ToolError: "));
    }
}
