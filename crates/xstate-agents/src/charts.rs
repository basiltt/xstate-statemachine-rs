//! The reference charts, embedded verbatim from the Python package (`charts/*.json`).
//!
//! * `tool_loop` -- `TOOL_LOOP`: the single-agent loop [`AgentLogic`](crate::AgentLogic)
//!   drives;
//! * `supervisor`, `pipeline`, `debate` -- multi-agent recipes (example charts, not runtimes)
//!   whose actions spawn `TOOL_LOOP` sub-agents (see [`crate::multi`]).

use serde_json::Value;

use crate::error::{AgentError, Result};

/// The `TOOL_LOOP` chart as JSON text. Prefer [`load_chart`] for a parsed copy.
pub const TOOL_LOOP_JSON: &str = include_str!("../charts/tool_loop.json");
/// The SUPERVISOR recipe as JSON text.
pub const SUPERVISOR_JSON: &str = include_str!("../charts/supervisor.json");
/// The PIPELINE recipe as JSON text.
pub const PIPELINE_JSON: &str = include_str!("../charts/pipeline.json");
/// The DEBATE recipe as JSON text.
pub const DEBATE_JSON: &str = include_str!("../charts/debate.json");

/// Names accepted by [`load_chart`].
pub const CHART_NAMES: &[&str] = &["tool_loop", "supervisor", "pipeline", "debate"];

/// A fresh parsed copy of a reference chart by name: `tool_loop`, `supervisor`, `pipeline`
/// or `debate`.
///
/// # Errors
/// [`AgentError::Config`] for an unknown name (`no reference chart named 'x'`).
pub fn load_chart(name: &str) -> Result<Value> {
    let text = match name {
        "tool_loop" => TOOL_LOOP_JSON,
        "supervisor" => SUPERVISOR_JSON,
        "pipeline" => PIPELINE_JSON,
        "debate" => DEBATE_JSON,
        other => {
            return Err(AgentError::config(format!(
                "no reference chart named {}",
                crate::pyfmt::py_repr_str(other)
            )));
        }
    };
    serde_json::from_str(text).map_err(|e| AgentError::config(format!("chart {name}: {e}")))
}

/// The `TOOL_LOOP` chart, parsed.
///
/// # Errors
/// Never in practice: the embedded chart is valid JSON (a unit test guards it).
pub fn tool_loop() -> Result<Value> {
    load_chart("tool_loop")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chart_parses_and_compiles() {
        for name in CHART_NAMES {
            let chart = load_chart(name).unwrap_or_else(|e| panic!("{e}"));
            xstate_core::Machine::from_value(&chart).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(tool_loop().unwrap_or_else(|e| panic!("{e}"))["id"], "toolLoop");
    }

    #[test]
    fn unknown_chart_is_a_config_error() {
        let err = load_chart("nope").err().unwrap_or_else(|| panic!("expected an error"));
        assert_eq!(err.to_string(), "no reference chart named 'nope'");
    }
}
