//! Provider-neutral model I/O: [`ToolCall`], [`Usage`], [`ModelResponse`], the
//! [`ModelCall`] trait and the scripted [`FakeModel`].
//!
//! The chart never sees a provider SDK object: an adapter maps its SDK's response into a
//! [`ModelResponse`], and everything downstream (guards, context, snapshots, traces) works on
//! its JSON form ([`ModelResponse::to_value`]), so a snapshot taken mid-conversation restores
//! without the SDK.
//!
//! Messages in `context["messages"]` use one neutral shape:
//! `{"role": "system"|"user"|"assistant"|"tool", "content": str, "tool_calls": [...]
//! (assistant only, optional), "tool_call_id": str, "name": str (tool only)}`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value, json};

use crate::budgets::EXHAUSTED;
use crate::error::{AgentError, Result};
use crate::pyfmt::py_str;

/// One tool invocation the model PROPOSED (not yet authorised).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Call id chosen by the model; approvals name it.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as proposed. Kept as-is even when not an object, so validation denies it.
    pub arguments: Value,
}

impl ToolCall {
    /// A call with object arguments.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self { id: id.into(), name: name.into(), arguments }
    }

    /// `{"id", "name", "arguments"}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({"id": self.id, "name": self.name, "arguments": self.arguments})
    }

    /// Rebuild from `{"id", "name", "arguments"}`; missing `arguments` become `{}`, a
    /// non-object is kept so the tool schema refuses it.
    ///
    /// # Errors
    /// [`AgentError::Config`] when `id` or `name` is missing.
    pub fn from_value(value: &Value) -> Result<Self> {
        let field = |k: &str| {
            value
                .get(k)
                .filter(|v| !v.is_null())
                .map(py_str)
                .ok_or_else(|| AgentError::config(format!("tool call without {k}")))
        };
        let arguments = match value.get("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(other) => other.clone(),
        };
        Ok(Self { id: field("id")?, name: field("name")?, arguments })
    }
}

/// Rebuild [`ToolCall`]s from `context["pending_tool_calls"]`.
///
/// # Errors
/// The first malformed entry.
pub fn calls_from(pending: &Value) -> Result<Vec<ToolCall>> {
    pending
        .as_array()
        .map_or(Ok(Vec::new()), |items| items.iter().map(ToolCall::from_value).collect())
}

/// Token and cost accounting for ONE model call.
///
/// Tokens may be negative here (a hostile provider); they are clamped when charged to the
/// budget. A value that is not a number is carried as a budget-exhausting amount.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cost in US dollars.
    pub cost_usd: f64,
}

impl Default for Usage {
    fn default() -> Self {
        Self { input_tokens: 0, output_tokens: 0, cost_usd: 0.0 }
    }
}

impl Usage {
    /// `{"input_tokens": int, "output_tokens": int, "cost_usd": float}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({"input_tokens": self.input_tokens, "output_tokens": self.output_tokens, "cost_usd": self.cost_usd})
    }

    /// Parse a provider usage mapping; garbage becomes a budget-exhausting amount (fail
    /// closed), `null` / missing becomes 0.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        Self {
            input_tokens: int_of(value.get("input_tokens")),
            output_tokens: int_of(value.get("output_tokens")),
            cost_usd: float_of(value.get("cost_usd")),
        }
    }
}

fn number_of(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => Some(0.0),
        Some(Value::String(s)) if s.is_empty() => Some(0.0),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(Value::Bool(b)) => Some(f64::from(u8::from(*b))),
        Some(_) => None,
    }
}

#[allow(clippy::cast_possible_truncation)] // clamped to +-EXHAUSTED first
fn int_of(value: Option<&Value>) -> i64 {
    match number_of(value) {
        Some(f) if f.is_finite() => f.clamp(-EXHAUSTED, EXHAUSTED).trunc() as i64,
        _ => EXHAUSTED as i64,
    }
}

fn float_of(value: Option<&Value>) -> f64 {
    number_of(value).filter(|f| f.is_finite()).unwrap_or(EXHAUSTED)
}

/// What a [`ModelCall`] returns: text and/or tool calls, plus usage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelResponse {
    /// Reply text.
    pub text: String,
    /// Proposed tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Usage of this call.
    pub usage: Usage,
    /// Model name.
    pub model: String,
}

impl ModelResponse {
    /// A text-only response.
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// `{"text", "tool_calls", "usage", "model"}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "text": self.text,
            "tool_calls": self.tool_calls.iter().map(ToolCall::to_value).collect::<Vec<_>>(),
            "usage": self.usage.to_value(),
            "model": self.model,
        })
    }

    /// Parse the JSON form.
    ///
    /// # Errors
    /// A malformed tool call.
    pub fn from_value(value: &Value) -> Result<Self> {
        let text_of = |k: &str| {
            value.get(k).filter(|v| crate::pyfmt::truthy(v)).map(py_str).unwrap_or_default()
        };
        Ok(Self {
            text: text_of("text"),
            tool_calls: calls_from(value.get("tool_calls").unwrap_or(&Value::Null))?,
            usage: Usage::from_value(value.get("usage").unwrap_or(&Value::Null)),
            model: text_of("model"),
        })
    }
}

/// One model request: the neutral messages and the tool descriptors (`{name, description,
/// parameters}`) the active state allows.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    /// Conversation so far (system prompt first, when configured).
    pub messages: Vec<Value>,
    /// Tools the model is told about.
    pub tools: Vec<Value>,
}

/// A model: `call(request) -> ModelResponse`, synchronous (the services run on
/// [`xstate_core::SyncInterpreter`]). Closures `Fn(&ModelRequest) -> Result<ModelResponse>`
/// implement it.
pub trait ModelCall: Send + Sync {
    /// Return the model's next response.
    ///
    /// # Errors
    /// Any failure; the chart routes it to `timed_out` and retries per the retry policy.
    fn call(&self, request: &ModelRequest) -> Result<ModelResponse>;
}

impl<F> ModelCall for F
where
    F: Fn(&ModelRequest) -> Result<ModelResponse> + Send + Sync,
{
    fn call(&self, request: &ModelRequest) -> Result<ModelResponse> {
        self(request)
    }
}

/// One item of a [`FakeModel`] script.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptItem {
    /// A JSON spec: `{"text": ...}`, `{"tool": name, "args": {...}}`, `{"tool_calls": [...]}`,
    /// each optionally with `"usage"` and `"id"`.
    Spec(Value),
    /// A ready response.
    Response(ModelResponse),
    /// An error to return.
    Error(AgentError),
}

/// A scripted [`ModelCall`] for tests, docs and simulation. Each call consumes the next
/// script item. Default usage is 10 in / 5 out / $0.0; every request is recorded (see
/// [`FakeModel::calls`]).
#[derive(Debug)]
pub struct FakeModel {
    script: Vec<ScriptItem>,
    name: String,
    default_usage: Value,
    nonce: String,
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    pos: usize,
    calls: Vec<Value>,
}

impl FakeModel {
    /// A model answering with the JSON specs of `script`, in order.
    pub fn new(script: impl IntoIterator<Item = Value>) -> Self {
        Self::from_items(script.into_iter().map(ScriptItem::Spec))
    }

    /// A model answering with arbitrary script items.
    pub fn from_items(script: impl IntoIterator<Item = ScriptItem>) -> Self {
        let mut hasher = RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        Self {
            script: script.into_iter().collect(),
            name: "fake".into(),
            default_usage: json!({"input_tokens": 10, "output_tokens": 5, "cost_usd": 0.0}),
            nonce: format!("{:08x}", hasher.finish() & 0xffff_ffff),
            state: Mutex::new(FakeState::default()),
        }
    }

    /// Set the reported model name (default `fake`).
    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Replace the default usage mapping.
    #[must_use]
    pub fn default_usage(mut self, usage: Value) -> Self {
        self.default_usage = usage;
        self
    }

    /// Every request so far: `{"messages": [...], "tools": [names]}`.
    pub fn calls(&self) -> Vec<Value> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).calls.clone()
    }

    /// Script items consumed so far.
    pub fn position(&self) -> usize {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).pos
    }

    /// A call id unique across instances: `call_<nonce>_<n>_<k>`.
    fn mint(&self, n: usize, k: usize) -> String {
        format!("call_{}_{n}_{k}", self.nonce)
    }

    fn build(&self, item: &Value, n: usize) -> Result<ModelResponse> {
        let mut usage = self.default_usage.clone();
        if let (Some(base), Some(extra)) =
            (usage.as_object_mut(), item.get("usage").and_then(Value::as_object))
        {
            base.extend(extra.clone());
        }
        let mut calls = Vec::new();
        if let Some(name) = item.get("tool") {
            let id = item
                .get("id")
                .filter(|v| crate::pyfmt::truthy(v))
                .map_or_else(|| self.mint(n, 0), py_str);
            calls.push(ToolCall::new(id, py_str(name), object_or_empty(item.get("args"))));
        }
        for (k, c) in
            item.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate()
        {
            let id = c
                .get("id")
                .filter(|v| crate::pyfmt::truthy(v))
                .map_or_else(|| self.mint(n, k), py_str);
            let name = c
                .get("name")
                .map(py_str)
                .ok_or_else(|| AgentError::config("tool_calls entry without name"))?;
            calls.push(ToolCall::new(id, name, object_or_empty(c.get("arguments"))));
        }
        Ok(ModelResponse {
            text: item.get("text").map(py_str).unwrap_or_default(),
            tool_calls: calls,
            usage: Usage::from_value(&usage),
            model: self.name.clone(),
        })
    }
}

fn object_or_empty(value: Option<&Value>) -> Value {
    match value {
        Some(v) if crate::pyfmt::truthy(v) => v.clone(),
        _ => Value::Object(Map::new()),
    }
}

impl ModelCall for FakeModel {
    fn call(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let n = {
            let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let names: Vec<Value> = request
                .tools
                .iter()
                .map(|t| t.get("name").cloned().unwrap_or(Value::Null))
                .collect();
            st.calls.push(json!({"messages": request.messages, "tools": names}));
            if st.pos >= self.script.len() {
                return Err(AgentError::Agent(format!(
                    "FakeModel script exhausted after {} call(s)",
                    st.pos
                )));
            }
            st.pos += 1;
            st.pos
        };
        match &self.script[n - 1] {
            ScriptItem::Error(err) => Err(err.clone()),
            ScriptItem::Response(resp) => Ok(resp.clone()),
            ScriptItem::Spec(item) => self.build(item, n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ModelRequest {
        ModelRequest {
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: vec![json!({"name": "t"})],
        }
    }

    #[test]
    fn fake_model_follows_its_script() {
        let m = FakeModel::new([
            json!({"text": "a"}),
            json!({"tool": "t", "args": {"x": 1}, "usage": {"cost_usd": 0.5}}),
            json!({"tool_calls": [{"name": "t"}, {"name": "u", "id": "fixed", "arguments": {"y": 2}}]}),
        ]);
        assert_eq!(m.call(&request()).unwrap_or_else(|e| panic!("{e}")).text, "a");
        let second = m.call(&request()).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            second.tool_calls[0].id.starts_with("call_")
                && second.tool_calls[0].id.ends_with("_2_0")
        );
        assert!((second.usage.cost_usd - 0.5).abs() < f64::EPSILON);
        assert_eq!(second.usage.input_tokens, 10);
        let third = m.call(&request()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(third.tool_calls[1].id, "fixed");
        assert_eq!(third.tool_calls[0].arguments, json!({}));
        let err = m.call(&request()).err().unwrap_or_else(|| panic!("expected exhaustion"));
        assert_eq!(err.to_string(), "FakeModel script exhausted after 3 call(s)");
        assert_eq!(m.calls().len(), 4);
        assert_eq!(m.calls()[0]["tools"], json!(["t"]));
        assert_eq!(m.position(), 3);
    }

    #[test]
    fn script_items_can_be_errors_and_responses() {
        let m = FakeModel::from_items([
            ScriptItem::Error(AgentError::Agent("down".into())),
            ScriptItem::Response(ModelResponse::text("ok")),
        ])
        .named("m1");
        assert!(m.call(&request()).is_err());
        assert_eq!(m.call(&request()).unwrap_or_else(|e| panic!("{e}")).text, "ok");
    }

    #[test]
    fn usage_fails_closed_on_garbage() {
        let u = Usage::from_value(
            &json!({"input_tokens": "nan", "output_tokens": "1.5", "cost_usd": "x"}),
        );
        assert_eq!(u.output_tokens, 1);
        assert!(u.input_tokens >= 1_000_000_000_000_000);
        assert!(u.cost_usd >= 1e15);
        assert_eq!(Usage::from_value(&Value::Null), Usage::default());
    }

    #[test]
    fn responses_round_trip() {
        let r = ModelResponse {
            text: "t".into(),
            tool_calls: vec![ToolCall::new("1", "x", json!([1]))],
            usage: Usage { input_tokens: 1, output_tokens: 2, cost_usd: 0.1 },
            model: "m".into(),
        };
        assert_eq!(ModelResponse::from_value(&r.to_value()).unwrap_or_else(|e| panic!("{e}")), r);
        assert!(ToolCall::from_value(&json!({"name": "x"})).is_err());
        assert_eq!(calls_from(&Value::Null).unwrap_or_else(|e| panic!("{e}")), vec![]);
    }

    #[test]
    fn closures_are_models() {
        let model = |_: &ModelRequest| Ok(ModelResponse::text("c"));
        assert_eq!(ModelCall::call(&model, &request()).unwrap_or_else(|e| panic!("{e}")).text, "c");
    }
}
