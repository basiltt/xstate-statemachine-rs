//! The `TOOL_LOOP` guards. They are advisory -- `runTool` re-checks everything -- but they are
//! what keeps a forbidden or malformed call from ever reaching a human or a tool.

use std::sync::Arc;

use serde_json::Value;
use xstate_core::{DynContext, Event, MachineLogic};

use super::{Core, data_of};
use crate::budgets::ctx_int;
use crate::error::split_event_error;
use crate::messages::calls_from;
use crate::output::{task_of, validate_output};
use crate::tools::approval::approval_matches;

/// The error text of an `xstate.error.actor.*` event.
pub(crate) fn event_error(event: &Event) -> Option<&str> {
    event.payload.get("error").and_then(Value::as_str)
}

impl Core {
    /// Every proposed call is permitted and well-formed: not too many, no duplicate ids,
    /// registered, allowed here, schema-valid.
    pub(crate) fn tool_allowed(&self, data: &Value) -> bool {
        if data.get("too_many_tool_calls").is_some_and(crate::pyfmt::truthy)
            || data.get("duplicate_call_ids").is_some_and(crate::pyfmt::truthy)
        {
            return false;
        }
        let allowed: Vec<&str> = data
            .get("allowed_tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let Ok(calls) = calls_from(data.get("tool_calls").unwrap_or(&Value::Null)) else {
            return false;
        };
        calls.iter().all(|c| {
            allowed.contains(&c.name.as_str())
                && self.tools.get(&c.name).is_some_and(|t| t.validate(&c.arguments).is_ok())
        })
    }

    fn needs_human(&self, data: &Value) -> bool {
        calls_from(data.get("tool_calls").unwrap_or(&Value::Null))
            .unwrap_or_default()
            .iter()
            .any(|c| self.tools.get(&c.name).is_some_and(crate::tools::Tool::is_side_effect))
    }

    pub(crate) fn output_valid(&self, data: &Value) -> (bool, Value) {
        let schema = self.output_model_for(data);
        let text = data.get("text").map(crate::pyfmt::py_str).unwrap_or_default();
        validate_output(schema.as_ref(), &text, self.output_parser.as_ref())
    }
}

/// Register the guards on `logic`.
pub(super) fn register(
    logic: MachineLogic<DynContext>,
    core: &Arc<Core>,
) -> MachineLogic<DynContext> {
    let (c1, c2, c3, c4) = (Arc::clone(core), Arc::clone(core), Arc::clone(core), Arc::clone(core));
    logic
        .guard("toolAllowed", move |_, e, _| Ok(c1.tool_allowed(data_of(e))))
        .guard("needsHuman", move |_, e, _| Ok(c2.needs_human(data_of(e))))
        .guard("hasToolCalls", |_, e, _| {
            Ok(data_of(e).get("tool_calls").is_some_and(crate::pyfmt::truthy))
        })
        .guard("canRetryOutput", move |ctx, _, _| {
            Ok(ctx_int(ctx, "output_retries") < i64::from(c3.max_output_retries))
        })
        .guard("isToolDenied", |_, e, _| {
            Ok(event_error(e).is_some_and(|t| split_event_error(t).0 == "ToolDeniedError"))
        })
        .guard("outputValid", move |_, e, _| Ok(c4.output_valid(data_of(e)).0))
        .guard("hasTask", |ctx, _, _| Ok(task_of(ctx).is_some()))
        .guard("approvalMatches", |ctx, e, _| Ok(approval_matches(ctx, &e.payload)))
}
