//! The retry trio the `TOOL_LOOP` chart's `timed_out` state uses: `retryBump` (entry),
//! `retryCanRetry` and the `retryDelay` delay, plus `retryReset`, over `context["attempt"]`.
//!
//! The Python logic merges `RetryPolicy(max_attempts=3, base_ms=1000, jitter="none")`. This
//! crate keeps its own jitter-free policy rather than depending on `xstate-patterns`, so the
//! agent crate only needs `xstate-core`; the delays are identical to the Python default.

use serde_json::Value;
use xstate_core::{ActionScope, DynContext, MachineLogic};

use super::set;
use crate::budgets::ctx_int;
use crate::error::{AgentError, Result};

/// Exponential backoff without jitter: `min(base_ms * factor^(attempt-1), max_ms)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentRetry {
    /// Total attempts allowed, including the first.
    pub max_attempts: u32,
    /// Delay after the first failure, in ms.
    pub base_ms: f64,
    /// Growth per attempt.
    pub factor: f64,
    /// Cap on any delay, in ms.
    pub max_ms: f64,
}

impl Default for AgentRetry {
    fn default() -> Self {
        Self { max_attempts: 3, base_ms: 1000.0, factor: 2.0, max_ms: 30_000.0 }
    }
}

impl AgentRetry {
    /// Refuse a policy that would produce NaN, infinite or zero delays.
    ///
    /// # Errors
    /// [`AgentError::Config`] unless `max_attempts >= 1` and `base_ms`, `factor` and `max_ms`
    /// are finite and positive.
    pub fn validate(&self) -> Result<()> {
        if self.max_attempts < 1 {
            return Err(AgentError::config("retry max_attempts must be >= 1"));
        }
        for (name, value) in
            [("base_ms", self.base_ms), ("factor", self.factor), ("max_ms", self.max_ms)]
        {
            if !(value.is_finite() && value > 0.0) {
                return Err(AgentError::config(format!(
                    "retry {name} must be a finite number > 0, got {value}"
                )));
            }
        }
        Ok(())
    }

    /// Delay before the next try after `attempt` (1-based) failed.
    #[must_use]
    pub fn delay_ms(&self, attempt: u32) -> f64 {
        let exp = i32::try_from(attempt.max(1) - 1).unwrap_or(i32::MAX);
        let raw = self.base_ms * self.factor.powi(exp);
        if raw.is_nan() { self.max_ms } else { raw.min(self.max_ms) }
    }

    /// Register `retryDelay`, `retryCanRetry`, `retryBump` and `retryReset` on `logic`.
    #[must_use]
    pub fn apply(&self, logic: MachineLogic<DynContext>) -> MachineLogic<DynContext> {
        let policy = *self;
        let max = i64::from(self.max_attempts);
        logic
            .delay_fn("retryDelay", move |ctx, _| {
                Ok(policy
                    .delay_ms(u32::try_from(ctx_int(ctx, "attempt").max(1)).unwrap_or(u32::MAX)))
            })
            .guard("retryCanRetry", move |ctx, _, _| Ok(ctx_int(ctx, "attempt") < max))
            .action("retryBump", |scope: &mut ActionScope<'_, DynContext>| {
                let next = ctx_int(scope.context, "attempt") + 1;
                set(scope.context, "attempt", Value::from(next));
                Ok(())
            })
            .action("retryReset", |scope: &mut ActionScope<'_, DynContext>| {
                set(scope.context, "attempt", Value::from(0));
                Ok(())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_non_finite_and_non_positive_fields() {
        assert!(AgentRetry::default().validate().is_ok());
        let bad = [
            AgentRetry { max_attempts: 0, ..AgentRetry::default() },
            AgentRetry { base_ms: f64::NAN, ..AgentRetry::default() },
            AgentRetry { base_ms: 0.0, ..AgentRetry::default() },
            AgentRetry { factor: f64::INFINITY, ..AgentRetry::default() },
            AgentRetry { factor: -2.0, ..AgentRetry::default() },
            AgentRetry { max_ms: f64::INFINITY, ..AgentRetry::default() },
        ];
        for retry in bad {
            assert!(retry.validate().is_err(), "{retry:?}");
        }
    }

    #[test]
    fn delays_grow_and_cap() {
        let r = AgentRetry::default();
        assert!((r.delay_ms(1) - 1000.0).abs() < f64::EPSILON);
        assert!((r.delay_ms(3) - 4000.0).abs() < f64::EPSILON);
        assert!((r.delay_ms(0) - 1000.0).abs() < f64::EPSILON);
        assert!((r.delay_ms(40) - 30_000.0).abs() < f64::EPSILON);
    }
}
