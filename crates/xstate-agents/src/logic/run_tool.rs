//! The `runTool` service: authorise EVERY pending call (allow-list, schema, approval) before
//! running ANY, then execute them one by one, each bounded by its timeout, scrubbing and
//! truncating the outputs before they enter the conversation.

use serde_json::{Map, Value, json};

use super::Core;
use crate::error::{AgentError, Result};
use crate::messages::{ToolCall, calls_from};
use crate::pyfmt::py_str;
use crate::scrub::scrub;

impl Core {
    fn authorise_all(&self, ctx: &Value) -> Result<Vec<(ToolCall, Value)>> {
        let approved: Vec<String> = ctx
            .get("approved_call_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(py_str)
            .collect();
        let calls = calls_from(ctx.get("pending_tool_calls").unwrap_or(&Value::Null))?;
        let mut plan = Vec::with_capacity(calls.len());
        for call in calls {
            match self.tools.authorise(&call, &self.tool_tools, &approved) {
                Ok(args) => plan.push((call, args)),
                Err(err) => {
                    if let AgentError::ToolDenied { reason, .. } = &err {
                        let mut fields = Map::new();
                        fields.insert("tool".into(), Value::String(call.name.clone()));
                        fields.insert("reason".into(), Value::String(reason.clone()));
                        self.trace("tool_denied", &self.tool_states, &fields);
                    }
                    return Err(err);
                }
            }
        }
        Ok(plan)
    }

    fn tool_result(&self, call: &ToolCall, value: &Value) -> Value {
        let safe = scrub(value, true);
        let mut fields = Map::new();
        fields.insert("tool".into(), Value::String(call.name.clone()));
        fields.insert("arguments".into(), call.arguments.clone());
        fields.insert("output".into(), safe.clone());
        self.trace("tool_call", &self.tool_states, &fields);
        json!({"tool_call_id": call.id, "name": call.name, "content": self.tools.truncate(&safe)})
    }

    /// Execute the authorised pending tool calls; return the tool-result messages.
    pub(crate) fn run_tool(&self, ctx: &Value) -> Result<Value> {
        let plan = self.authorise_all(ctx)?;
        let mut out = Vec::with_capacity(plan.len());
        for (call, args) in plan {
            let value = self.tools.call(&call, args)?;
            out.push(self.tool_result(&call, &value));
        }
        Ok(Value::Array(out))
    }
}
