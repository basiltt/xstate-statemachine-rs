//! The `callModel` service: build the request from the transcript and the hosting state's
//! allow-list, call the model, and bound what the (possibly injected) response can put into
//! context.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::Core;
use crate::error::Result;
use crate::messages::ModelRequest;
use crate::pyfmt::{py_str, truthy};
use crate::scrub::scrub;

impl Core {
    fn model_request(&self) -> (Vec<String>, Map<String, Value>) {
        let allowed = self.model_tools.clone();
        let mut extra = Map::new();
        let names: Vec<Value> =
            self.tools.expand(&allowed).into_iter().map(Value::String).collect();
        extra.insert("allowed_tools".into(), Value::Array(names));
        if let Some(spec) = &self.model_output_spec {
            extra.insert("output_model".into(), spec.clone());
        }
        (allowed, extra)
    }

    /// Call the model synchronously; return the normalised, bounded response data.
    pub(crate) fn call_model(&self, ctx: &Value) -> Result<Value> {
        let (allowed, extra) = self.model_request();
        let mut messages: Vec<Value> =
            ctx.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(system) = self.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            messages.insert(0, serde_json::json!({"role": "system", "content": system}));
        }
        let request = ModelRequest { messages, tools: self.tools.schemas(&allowed) };
        let response = self.model.call(&request)?;
        let mut raw = response.to_value();
        if let Value::Object(map) = &mut raw {
            map.extend(extra);
        }
        let mut fields = Map::new();
        fields.insert("response".into(), raw.clone());
        fields.insert("messages".into(), Value::Array(request.messages));
        self.trace("model_call", &self.model_states, &fields);
        Ok(flag_reused_ids(self.bounded(raw), ctx))
    }

    /// Cap the response: text truncated like tool output, arguments value-scrubbed, too
    /// many calls or duplicate ids flagged (and later denied by `toolAllowed`).
    fn bounded(&self, mut data: Value) -> Value {
        let Value::Object(map) = &mut data else { return data };
        let text = map
            .get("text")
            .filter(|t| truthy(t))
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        map.insert("text".into(), Value::String(self.tools.truncate(&text)));
        let mut calls: Vec<Value> =
            map.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if calls.len() > self.max_tool_calls {
            map.insert("too_many_tool_calls".into(), Value::from(calls.len()));
            calls.truncate(self.max_tool_calls);
        }
        let ids: Vec<Value> =
            calls.iter().map(|c| c.get("id").cloned().unwrap_or(Value::Null)).collect();
        let distinct: HashSet<String> = ids.iter().map(py_str).collect();
        if distinct.len() != ids.len() {
            map.insert("duplicate_call_ids".into(), Value::Bool(true));
        }
        for call in &mut calls {
            if let Value::Object(c) = call {
                let args = c
                    .get("arguments")
                    .filter(|a| truthy(a))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                c.insert("arguments".into(), scrub(&args, false));
            }
        }
        map.insert("tool_calls".into(), Value::Array(calls));
        data
    }
}

/// Approval is by call id and ids are chosen by the model: re-using an id from an earlier
/// batch (remembered in `spent_call_ids`, or still in the transcript) is flagged so the turn
/// is denied.
fn flag_reused_ids(mut data: Value, ctx: &Value) -> Value {
    let mut seen: HashSet<String> = ctx
        .get("spent_call_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(py_str)
        .collect();
    for msg in ctx.get("messages").and_then(Value::as_array).into_iter().flatten() {
        for call in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            if call.is_object() {
                seen.insert(call.get("id").map_or_else(|| "None".into(), py_str));
            }
        }
    }
    let reused = data.get("tool_calls").and_then(Value::as_array).is_some_and(|calls| {
        calls.iter().any(|c| seen.contains(&c.get("id").map(py_str).unwrap_or_default()))
    });
    if reused && let Value::Object(map) = &mut data {
        map.insert("duplicate_call_ids".into(), Value::Bool(true));
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reused_ids_are_flagged() {
        let ctx = json!({"spent_call_ids": ["a"], "messages": [{"role": "assistant", "tool_calls": [{"id": "b"}]}]});
        let flagged = |id: &str| {
            flag_reused_ids(json!({"tool_calls": [{"id": id}]}), &ctx)
                .get("duplicate_call_ids")
                .is_some()
        };
        assert!(flagged("a"));
        assert!(flagged("b"));
        assert!(!flagged("c"));
    }
}
