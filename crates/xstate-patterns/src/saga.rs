//! [`SagaBuilder`]: orchestrated sagas as *plain* statechart JSON.
//!
//! Every saga tutorial hand-builds the same machine: a step ledger, a per-step timeout,
//! compensation in reverse order, retries. Those are exactly `invoke` + `after` + `onError` +
//! context. The builder emits the JSON a human would have written -- no runtime magic,
//! Stately-editable -- plus [`SagaBuilder::logic`], the handful of generic actions it
//! references. Your services stay yours.
//!
//! Shape for steps a, b, c (each `steps.<name>` invokes its service):
//!
//! ```text
//! steps.a --done--> steps.b --done--> steps.c --done--> completed
//!    |error/timeout   |error/timeout    |error/timeout
//!    v                v                 v
//!  failed        compensating.a    compensating.b -> compensating.a
//!                     |                                  |
//!                     +------------> failed <------------+
//! ```
//!
//! * a step with a [`RetryPolicy`] fails into `steps.<name>Retrying`, which waits the policy's
//!   delay and re-enters the step while `<step>RetryCanRetry` holds, else compensates;
//! * only steps that declared `compensate` get a compensating state; compensation walks the
//!   *completed* steps in reverse, each exactly once (entering a state once runs its invoke
//!   once);
//! * a compensation that itself fails ends in `compensationFailed`, tagged `dead-letter` so a
//!   dead-letter observer captures it;
//! * every step transition carries `meta.publish`, so an outbox observer can emit
//!   `<saga>.<step>.completed` / `.failed` / `.compensated` integration events.
//!
//! ```
//! use xstate_core::{Machine, SyncInterpreter};
//! use xstate_patterns::{SagaBuilder, SagaStep};
//!
//! let saga = SagaBuilder::new("order")?
//!     .step(SagaStep::new("reserve", "reserveStock").compensate("releaseStock"))?
//!     .step(SagaStep::new("charge", "chargeCard"))?;
//! let logic = saga
//!     .logic()
//!     .service("reserveStock", |_ctx, _input| Ok(serde_json::json!("r-1")))
//!     .service("releaseStock", |_ctx, _input| Ok(serde_json::Value::Null))
//!     .service("chargeCard", |_ctx, _input| Ok(serde_json::json!("c-1")));
//! let mut interp = SyncInterpreter::new(saga.machine()?, logic);
//! interp.start()?;
//! assert_eq!(interp.state_ids(), ["order.completed"]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use serde_json::{Map, Value, json};
use xstate_core::{ActionScope, DynContext, Error as EngineError, Machine, MachineLogic};

use crate::ctx::set_key;
use crate::retry::RetryPolicy;

/// Prefixes of events only the engine mints; a start event named like one could never be sent
/// by a caller.
const ENGINE_PREFIXES: [&str; 4] = ["done.", "error.", "xstate.", "after."];

/// Why a saga declaration or build failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SagaError {
    /// A name, key, timeout or event in the declaration is not acceptable.
    #[error("{0}")]
    Invalid(String),
    /// The engine rejected the generated chart.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

fn invalid(message: impl Into<String>) -> SagaError {
    SagaError::Invalid(message.into())
}

/// Refuse an empty / blank service or action key at declaration.
fn check_key(what: &str, key: &str) -> Result<(), SagaError> {
    if key.trim().is_empty() {
        return Err(invalid(format!("{what} must be a non-empty string, got {key:?}")));
    }
    Ok(())
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Refuse a start event a caller could never deliver.
fn check_event(event: &str) -> Result<(), SagaError> {
    check_key("start_event", event)?;
    if ENGINE_PREFIXES.iter().any(|p| event.starts_with(p)) {
        return Err(invalid(format!("start_event {event:?} is an engine-reserved event name")));
    }
    if event.contains('*') || event.chars().any(char::is_whitespace) {
        // A wildcard or whitespace name is never a concrete event a producer sends.
        return Err(invalid(format!(
            "start_event {event:?} must be a concrete event name (no wildcards or whitespace)"
        )));
    }
    Ok(())
}

/// `reserve_stock` -> `reserveStock`.
fn camel(name: &str) -> String {
    let mut parts = name.split('_');
    let mut out = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// One step: invoke `invoke`; undo with `compensate` on a later failure.
#[derive(Debug, Clone)]
pub struct SagaStep {
    /// Step identifier; also the `invoke` id and the middle of its event types.
    pub name: String,
    /// Service key that performs the step. Its output lands in `context.results[name]`;
    /// failing fails the step.
    pub invoke: String,
    /// Service key that undoes the step after a *later* step fails. Must be idempotent: a
    /// restart mid-compensation runs it again.
    pub compensate: Option<String>,
    /// Fail the step after this many milliseconds (positive).
    pub timeout_ms: Option<u64>,
    /// Re-run a failed or timed-out step per this policy before compensating.
    pub retry: Option<RetryPolicy>,
}

impl SagaStep {
    /// A step with no compensation, timeout or retry.
    #[must_use]
    pub fn new(name: impl Into<String>, invoke: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            invoke: invoke.into(),
            compensate: None,
            timeout_ms: None,
            retry: None,
        }
    }

    /// Set the compensating service.
    #[must_use]
    pub fn compensate(mut self, service: impl Into<String>) -> Self {
        self.compensate = Some(service.into());
        self
    }

    /// Fail the step after `ms` milliseconds.
    #[must_use]
    pub const fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Retry a failed step per `policy`.
    #[must_use]
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// Prefix of this step's retry logic names (`reserveStockRetry`).
    #[must_use]
    pub fn retry_prefix(&self) -> String {
        format!("{}Retry", camel(&self.name))
    }

    /// Context key holding this step's attempt counter (`attempt_<name>`).
    #[must_use]
    pub fn attempt_key(&self) -> String {
        format!("attempt_{}", self.name)
    }
}

/// Fluent builder for an orchestrated saga chart.
#[derive(Debug, Clone)]
pub struct SagaBuilder {
    name: String,
    start_event: Option<String>,
    steps: Vec<SagaStep>,
    on_failure: Vec<String>,
}

impl SagaBuilder {
    /// A saga called `name` (the machine id and event-type prefix); it starts its first step as
    /// soon as the interpreter starts.
    ///
    /// # Errors
    /// [`SagaError::Invalid`] when `name` is not an identifier.
    pub fn new(name: impl Into<String>) -> Result<Self, SagaError> {
        let name = name.into();
        if !is_identifier(&name) {
            return Err(invalid(format!("saga name {name:?} must be an identifier")));
        }
        Ok(Self { name, start_event: None, steps: Vec::new(), on_failure: Vec::new() })
    }

    /// Wait in `idle` for this event (the shape an inbound dispatcher drives) and keep its
    /// payload in `context.input` for the services.
    ///
    /// # Errors
    /// [`SagaError::Invalid`] for an empty, engine-reserved, wildcard or whitespace name.
    pub fn start_event(mut self, event: impl Into<String>) -> Result<Self, SagaError> {
        let event = event.into();
        check_event(&event)?;
        self.start_event = Some(event);
        Ok(self)
    }

    /// Append a step (steps run in declaration order).
    ///
    /// # Errors
    /// [`SagaError::Invalid`] for a non-identifier or duplicate name, a name that collides with
    /// a generated `<step>Retrying` state, an empty service key, or a zero `timeout_ms`.
    pub fn step(mut self, step: SagaStep) -> Result<Self, SagaError> {
        self.check_step_name(&step.name)?;
        if step.retry.is_some()
            && self.steps.iter().any(|s| s.name == format!("{}Retrying", step.name))
        {
            return Err(invalid(format!(
                "step {:?} with retry collides with step '{}Retrying' (its generated waiting state)",
                step.name, step.name
            )));
        }
        check_key("invoke", &step.invoke)?;
        if let Some(compensate) = &step.compensate {
            check_key("compensate", compensate)?;
        }
        if step.timeout_ms == Some(0) {
            return Err(invalid("timeout_ms must be a positive int, got 0"));
        }
        self.steps.push(step);
        Ok(self)
    }

    fn check_step_name(&self, name: &str) -> Result<(), SagaError> {
        if !is_identifier(name) {
            return Err(invalid(format!("step name {name:?} must be an identifier")));
        }
        let taken = self.steps.iter().any(|s| {
            s.name == name || (s.retry.is_some() && name == format!("{}Retrying", s.name))
        });
        if taken {
            return Err(invalid(format!("duplicate step {name:?}")));
        }
        Ok(())
    }

    /// Actions run on entering `failed` (after compensation).
    ///
    /// # Errors
    /// [`SagaError::Invalid`] for an empty action name.
    pub fn on_failure<I, S>(mut self, actions: I) -> Result<Self, SagaError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for action in actions {
            let action = action.into();
            check_key("on_failure action", &action)?;
            self.on_failure.push(action);
        }
        Ok(self)
    }

    /// The saga's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The start event, when the saga waits in `idle`.
    #[must_use]
    pub fn start_event_name(&self) -> Option<&str> {
        self.start_event.as_deref()
    }

    /// The declared steps, in order.
    #[must_use]
    pub fn steps(&self) -> &[SagaStep] {
        &self.steps
    }

    /// Every service key the chart invokes (steps, then compensations), without duplicates.
    /// Register an implementation for each on [`logic`](Self::logic).
    #[must_use]
    pub fn required_services(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let all = self
            .steps
            .iter()
            .map(|s| s.invoke.as_str())
            .chain(self.steps.iter().filter_map(|s| s.compensate.as_deref()));
        for key in all {
            if !out.contains(&key) {
                out.push(key);
            }
        }
        out
    }

    /// The saga as plain XState JSON (a new value each call).
    ///
    /// # Errors
    /// [`SagaError::Invalid`] when no step was declared.
    pub fn build(&self) -> Result<Value, SagaError> {
        let Some(first) = self.steps.first() else {
            return Err(invalid("a saga needs at least one step"));
        };
        let mut states = Map::new();
        if let Some(start) = &self.start_event {
            states.insert(
                "idle".into(),
                json!({ "on": { start: { "target": "steps", "actions": ["sagaStart"] } } }),
            );
        }
        states
            .insert("steps".into(), json!({ "initial": first.name, "states": self.step_states() }));
        let compensation = self.compensation_states();
        if let Some((initial, _)) = compensation.iter().next() {
            states.insert(
                "compensating".into(),
                json!({ "initial": initial, "states": compensation }),
            );
        }
        states.insert("completed".into(), json!({ "type": "final", "tags": ["publish"] }));
        states.insert("failed".into(), self.failed_state());
        states.insert(
            "compensationFailed".into(),
            json!({ "type": "final", "tags": ["dead-letter", "publish"] }),
        );
        Ok(json!({
            "id": self.name,
            "initial": if self.start_event.is_some() { "idle" } else { "steps" },
            "context": self.initial_context(),
            "states": states
        }))
    }

    fn initial_context(&self) -> Value {
        let mut ctx = Map::new();
        ctx.insert("results".into(), json!({}));
        ctx.insert("compensated".into(), json!([]));
        ctx.insert("error".into(), Value::Null);
        for step in self.steps.iter().filter(|s| s.retry.is_some()) {
            ctx.insert(step.attempt_key(), json!(0));
        }
        // `sagaStart` keeps the start event's payload here; the key exists in BOTH shapes so a
        // saga that starts immediately reads null, never a missing key.
        ctx.insert("input".into(), Value::Null);
        Value::Object(ctx)
    }

    fn failed_state(&self) -> Value {
        let mut failed = Map::new();
        failed.insert("type".into(), json!("final"));
        failed.insert("tags".into(), json!(["publish"]));
        if !self.on_failure.is_empty() {
            failed.insert("entry".into(), json!(self.on_failure));
        }
        Value::Object(failed)
    }

    /// [`build`](Self::build) compiled into an engine [`Machine`].
    ///
    /// # Errors
    /// [`SagaError::Invalid`] without steps; [`SagaError::Engine`] if the engine rejects the
    /// chart.
    pub fn machine(&self) -> Result<Machine, SagaError> {
        Ok(Machine::from_value(&self.build()?)?)
    }

    fn state_ref(&self, path: &str) -> String {
        format!("#{}.{path}", self.name)
    }

    fn publish(&self, step: &str, what: &str) -> Value {
        json!({ "publish": { "type": format!("{}.{step}.{what}", self.name) } })
    }

    /// Where a failure of step `k` goes: the latest compensable completed step's compensation,
    /// else `failed`.
    fn failure_target(&self, k: usize) -> String {
        self.steps[..k].iter().rev().find(|s| s.compensate.is_some()).map_or_else(
            || self.state_ref("failed"),
            |s| self.state_ref(&format!("compensating.{}", s.name)),
        )
    }

    fn step_states(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for (k, step) in self.steps.iter().enumerate() {
            out.insert(step.name.clone(), self.step_node(k, step));
            if step.retry.is_some() {
                out.insert(format!("{}Retrying", step.name), self.retrying_node(k, step));
            }
        }
        out
    }

    fn step_node(&self, k: usize, step: &SagaStep) -> Value {
        let next = self.steps.get(k + 1).map_or_else(
            || self.state_ref("completed"),
            |s| self.state_ref(&format!("steps.{}", s.name)),
        );
        let mut ok_actions = vec![json!({ "type": "sagaRecord", "params": { "step": step.name } })];
        if step.retry.is_some() {
            ok_actions.push(json!(format!("{}Reset", step.retry_prefix())));
        }
        let fail_target = if step.retry.is_some() {
            self.state_ref(&format!("steps.{}Retrying", step.name))
        } else {
            self.failure_target(k)
        };
        let mut node = json!({
            "invoke": {
                "id": step.name,
                "src": step.invoke,
                "onDone": {
                    "target": next,
                    "actions": ok_actions,
                    "meta": self.publish(&step.name, "completed")
                },
                "onError": self.failing(step, &fail_target, "error")
            }
        });
        if let Some(ms) = step.timeout_ms {
            node["after"] = json!({ ms.to_string(): self.failing(step, &fail_target, "timeout") });
        }
        node
    }

    /// The failure transition of a step: to `target`, running `sagaFail` (preceded by the retry
    /// bump when the step retries).
    fn failing(&self, step: &SagaStep, target: &str, reason: &str) -> Value {
        let mut actions = Vec::new();
        if step.retry.is_some() {
            actions.push(json!(format!("{}Bump", step.retry_prefix())));
        }
        actions.push(json!({
            "type": "sagaFail",
            "params": { "step": step.name, "reason": reason }
        }));
        json!({
            "target": target,
            "actions": actions,
            "meta": self.publish(&step.name, "failed")
        })
    }

    fn retrying_node(&self, k: usize, step: &SagaStep) -> Value {
        let prefix = step.retry_prefix();
        json!({
            "after": {
                format!("{prefix}Delay"): [
                    { "guard": format!("{prefix}CanRetry"), "target": step.name },
                    { "target": self.failure_target(k) }
                ]
            }
        })
    }

    /// Latest compensable step first (the entry order of a rollback).
    fn compensation_states(&self) -> Map<String, Value> {
        let compensable: Vec<&SagaStep> =
            self.steps.iter().filter(|s| s.compensate.is_some()).collect();
        let mut out = Map::new();
        for (idx, step) in compensable.iter().enumerate().rev() {
            let next = match idx.checked_sub(1) {
                Some(prev) => self.state_ref(&format!("compensating.{}", compensable[prev].name)),
                None => self.state_ref("failed"),
            };
            out.insert(step.name.clone(), self.compensation_node(step, &next));
        }
        out
    }

    fn compensation_node(&self, step: &SagaStep, next: &str) -> Value {
        json!({
            "invoke": {
                "id": format!("compensate_{}", step.name),
                "src": step.compensate,
                "onDone": {
                    "target": next,
                    "actions": [{ "type": "sagaCompensated", "params": { "step": step.name } }],
                    "meta": self.publish(&step.name, "compensated")
                },
                "onError": {
                    "target": self.state_ref("compensationFailed"),
                    "actions": [{
                        "type": "sagaFail",
                        "params": { "step": step.name, "reason": "compensation" }
                    }]
                }
            }
        })
    }

    /// The generic actions (and per-step retry logic) the JSON names. Bind your services on
    /// the result: `saga.logic().service("chargeCard", |ctx, input| ...)`; see
    /// [`required_services`](Self::required_services) for the keys, and register any
    /// [`on_failure`](Self::on_failure) actions too.
    #[must_use]
    pub fn logic(&self) -> MachineLogic<DynContext> {
        let mut logic = MachineLogic::dynamic()
            .action("sagaStart", |scope| {
                saga_start(scope);
                Ok(())
            })
            .action("sagaRecord", saga_record)
            .action("sagaFail", saga_fail)
            .action("sagaCompensated", saga_compensated);
        for step in &self.steps {
            if let Some(policy) = &step.retry {
                logic = policy.apply(logic, &step.retry_prefix(), &step.attempt_key());
            }
        }
        logic
    }
}

fn step_param(scope: &ActionScope<'_, DynContext>) -> xstate_core::Result<String> {
    scope
        .params
        .get("step")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EngineError::logic("saga action needs a string params.step"))
}

fn saga_start(scope: &mut ActionScope<'_, DynContext>) {
    let input =
        if scope.event.payload.is_object() { scope.event.payload.clone() } else { Value::Null };
    set_key(scope.context, "input", input);
}

fn saga_record(scope: &mut ActionScope<'_, DynContext>) -> xstate_core::Result<()> {
    let step = step_param(scope)?;
    let output = scope.event.output().clone();
    if let Some(Value::Object(results)) = scope.context.get_mut("results") {
        results.insert(step, output);
    } else {
        set_key(scope.context, "results", json!({ step: output }));
    }
    Ok(())
}

fn saga_fail(scope: &mut ActionScope<'_, DynContext>) -> xstate_core::Result<()> {
    let step = step_param(scope)?;
    let reason = scope.params.get("reason").cloned().unwrap_or(Value::Null);
    // The engine hands a failed service's error over as its message text.
    let error = scope.event.payload.get("error").filter(|e| !e.is_null()).cloned();
    let (kind, message) = match error {
        Some(message) => (json!("Error"), message),
        None => (Value::Null, Value::Null),
    };
    let record = json!({ "step": step, "reason": reason, "type": kind, "message": message });
    set_key(scope.context, "error", record);
    Ok(())
}

fn saga_compensated(scope: &mut ActionScope<'_, DynContext>) -> xstate_core::Result<()> {
    let step = Value::from(step_param(scope)?);
    if let Some(Value::Array(done)) = scope.context.get_mut("compensated") {
        done.push(step);
    } else {
        set_key(scope.context, "compensated", Value::Array(vec![step]));
    }
    Ok(())
}
