//! The `TOOL_LOOP` actions: transcript bookkeeping, usage accounting, approvals, denials and
//! failure records. Every write uses the Python context key names.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use xstate_core::{ActionScope, DynContext, Event, MachineLogic};

use super::guards::event_error;
use super::{Core, SPENT_IDS, data_of, set};
use crate::budgets::{ctx_float, ctx_int, spent_tokens, spent_usd, usage_totals};
use crate::error::{Result, engine_err, split_event_error};
use crate::messages::calls_from;
use crate::output::{retry_prompt, task_of};
use crate::pyfmt::{py_repr_str, py_str, truthy};
use crate::tools::approval::pending_ids;

type Scope<'a, 'b> = &'a mut ActionScope<'b, DynContext>;

fn fail(ctx: &mut Value, kind: &str, message: &str) {
    set(ctx, "error", json!({"kind": kind, "message": message}));
}

fn array(ctx: &Value, key: &str) -> Vec<Value> {
    ctx.get(key).and_then(Value::as_array).cloned().unwrap_or_default()
}

impl Core {
    fn append_user(&self, s: Scope<'_, '_>) -> Result<()> {
        let prompt = s
            .event
            .payload
            .get("prompt")
            .filter(|p| !p.is_null())
            .map(py_str)
            .or_else(|| task_of(s.context));
        match prompt {
            Some(p) => self.append(s.context, [json!({"role": "user", "content": p})]),
            None => Ok(()),
        }
    }

    fn record_response(&self, s: Scope<'_, '_>) -> Result<()> {
        let d = data_of(s.event).clone();
        let calls = array(&d, "tool_calls");
        let ctx = &mut *s.context;
        let mut ring: Vec<Value> = array(ctx, "spent_call_ids");
        ring.extend(calls.iter().filter_map(|c| {
            c.get("id").filter(|v| !v.is_null()).map(|v| Value::String(py_str(v)))
        }));
        let keep = ring.len().saturating_sub(SPENT_IDS);
        set(ctx, "spent_call_ids", Value::Array(ring.split_off(keep)));
        let usage = d.get("usage").cloned().unwrap_or(Value::Null);
        let field = |k: &str| usage.get(k).cloned().unwrap_or(Value::Null);
        set(ctx, "turns", Value::from(ctx_int(ctx, "turns") + 1));
        set(
            ctx,
            "tokens_in",
            Value::from(
                ctx_int(ctx, "tokens_in").saturating_add(spent_tokens(&field("input_tokens"))),
            ),
        );
        set(
            ctx,
            "tokens_out",
            Value::from(
                ctx_int(ctx, "tokens_out").saturating_add(spent_tokens(&field("output_tokens"))),
            ),
        );
        set(
            ctx,
            "cost_usd",
            Value::from(ctx_float(ctx, "cost_usd") + spent_usd(&field("cost_usd"))),
        );
        set(ctx, "attempt", Value::from(0));
        set(ctx, "approved_call_ids", json!([]));
        set(ctx, "pending_tool_calls", Value::Array(calls.clone()));
        let mut msg = json!({"role": "assistant", "content": d.get("text").cloned().unwrap_or_else(|| json!(""))});
        if !calls.is_empty() {
            msg["tool_calls"] = Value::Array(calls);
        }
        self.append(ctx, [msg])
    }

    fn append_tool_results(&self, s: Scope<'_, '_>) -> Result<()> {
        let results = s.event.output().as_array().cloned().unwrap_or_default();
        let msgs = results.into_iter().map(|r| {
            let mut m = Map::new();
            m.insert("role".into(), json!("tool"));
            if let Value::Object(fields) = r {
                m.extend(fields);
            }
            Value::Object(m)
        });
        self.append(s.context, msgs.collect::<Vec<_>>())?;
        set(s.context, "pending_tool_calls", json!([]));
        set(s.context, "approved_call_ids", json!([]));
        Ok(())
    }

    fn reject(&self, s: Scope<'_, '_>) -> Result<()> {
        let reason = s.event.payload.get("reason").map(py_str).unwrap_or_default();
        let content = format!("rejected by a human reviewer. {reason}").trim().to_owned();
        let msgs: Vec<Value> = array(s.context, "pending_tool_calls")
            .iter()
            .map(|c| json!({"role": "tool", "tool_call_id": c.get("id"), "name": c.get("name"), "content": content}))
            .collect();
        self.append(s.context, msgs)?;
        set(s.context, "pending_tool_calls", json!([]));
        set(s.context, "approved_call_ids", json!([]));
        set(s.context, "human_approved", Value::Bool(false));
        Ok(())
    }

    /// The last validation failure: field names + messages, never values.
    fn detail(&self, event: &Event) -> Value {
        let (ok, detail) = self.output_valid(data_of(event));
        if ok { json!("output rejected") } else { detail }
    }

    fn retry_output(&self, s: Scope<'_, '_>) -> Result<()> {
        let schema = self.output_model_for(data_of(s.event));
        let detail = self.detail(s.event);
        set(s.context, "output_retries", Value::from(ctx_int(s.context, "output_retries") + 1));
        self.append(s.context, [retry_prompt(schema.as_ref(), &detail)])
    }

    fn deny(&self, s: Scope<'_, '_>) {
        let ctx = &mut *s.context;
        if let Some(text) = event_error(s.event) {
            let (class, message) = split_event_error(text);
            if class == "ToolDeniedError" {
                fail(ctx, "tool_denied", message);
                return;
            }
        }
        let d = data_of(s.event);
        if let Some(n) = d.get("too_many_tool_calls").filter(|v| truthy(v)) {
            fail(
                ctx,
                "tool_denied",
                &format!("{n} tool calls in one turn (max_tool_calls={})", self.max_tool_calls),
            );
        } else if d.get("duplicate_call_ids").is_some_and(truthy) {
            fail(ctx, "tool_denied", "duplicate tool call ids");
        } else {
            self.deny_calls(ctx, d);
        }
        set(ctx, "pending_tool_calls", json!([]));
    }

    fn deny_calls(&self, ctx: &mut Value, d: &Value) {
        let allowed: Vec<&str> = d
            .get("allowed_tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let calls = calls_from(d.get("tool_calls").unwrap_or(&Value::Null)).unwrap_or_default();
        let bad: Vec<String> = calls
            .iter()
            .filter(|c| !self.tools.contains(&c.name) || !allowed.contains(&c.name.as_str()))
            .map(|c| py_repr_str(&c.name))
            .collect();
        if !bad.is_empty() {
            fail(ctx, "tool_denied", &format!("tool(s) [{}] not allowed here", bad.join(", ")));
            return;
        }
        for c in &calls {
            if let Some(tool) = self.tools.get(&c.name)
                && let Err(err) = tool.validate(&c.arguments)
            {
                fail(ctx, "tool_denied", &err.to_string());
                return;
            }
        }
    }

    fn notify_parent(&self, s: Scope<'_, '_>) {
        let failed = s.context.get("error").is_some_and(|e| !e.is_null());
        s.emit(json!({
            "type": if failed { "AGENT_FAILED" } else { "AGENT_DONE" },
            "agent_id": self.identity.agent_id,
            "usage": usage_totals(s.context),
            "result": s.context.get("result").cloned().unwrap_or(Value::Null),
            "error": s.context.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
}

fn record_failure(s: Scope<'_, '_>) {
    let class = event_error(s.event).map_or("unknown", |t| split_event_error(t).0);
    set(s.context, "last_failure", json!(class));
    set(s.context, "approved_call_ids", json!([]));
}

fn approve(s: Scope<'_, '_>) {
    let ids: Vec<Value> = pending_ids(s.context).into_iter().map(Value::String).collect();
    set(s.context, "approved_call_ids", Value::Array(ids));
    set(s.context, "human_approved", Value::Bool(true));
}

/// Fixed-kind failure actions: `(action name, error kind, message)`.
const FAILURES: &[(&str, &str, &str)] = &[
    ("recordModelTimeout", "timeout", "model call timed out"),
    ("recordToolTimeout", "timeout", "tool call timed out"),
    ("failTurnLimit", "budget", "turn limit reached"),
    ("failTokenBudget", "budget", "token budget exhausted"),
    ("failCostBudget", "budget", "cost budget exhausted"),
    ("failRetries", "retries", "retries exhausted"),
    ("escalateHuman", "human_timeout", "no human decision in time"),
];

/// Register the actions on `logic`.
pub(super) fn register(
    logic: MachineLogic<DynContext>,
    core: &Arc<Core>,
) -> MachineLogic<DynContext> {
    let c = |c: &Arc<Core>| Arc::clone(c);
    let (c1, c2, c3, c4, c5, c6, c7, c8, c9) =
        (c(core), c(core), c(core), c(core), c(core), c(core), c(core), c(core), c(core));
    let mut logic = logic
        .action("appendUserMessage", move |s: Scope<'_, '_>| engine_err(c1.append_user(s)))
        .action("recordModelResponse", move |s: Scope<'_, '_>| engine_err(c2.record_response(s)))
        .action("appendToolResults", move |s: Scope<'_, '_>| engine_err(c3.append_tool_results(s)))
        .action("approveHuman", |s: Scope<'_, '_>| {
            approve(s);
            Ok(())
        })
        .action("rejectPending", move |s: Scope<'_, '_>| engine_err(c4.reject(s)))
        .action("storeResult", move |s: Scope<'_, '_>| {
            let (_, value) = c5.output_valid(data_of(s.event));
            set(s.context, "result", value);
            Ok(())
        })
        .action("retryOutput", move |s: Scope<'_, '_>| engine_err(c6.retry_output(s)))
        .action("failOutput", move |s: Scope<'_, '_>| {
            let detail = py_str(&c7.detail(s.event));
            fail(s.context, "output", &format!("model output failed validation ({detail})"));
            Ok(())
        })
        .action("denyTool", move |s: Scope<'_, '_>| {
            c8.deny(s);
            Ok(())
        })
        .action("recordFailure", |s: Scope<'_, '_>| {
            record_failure(s);
            Ok(())
        })
        .action("notifyParent", move |s: Scope<'_, '_>| {
            c9.notify_parent(s);
            Ok(())
        });
    for &(name, kind, message) in FAILURES {
        logic = logic.action(name, move |s: Scope<'_, '_>| {
            fail(s.context, kind, message);
            Ok(())
        });
    }
    logic
}
