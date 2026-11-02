//! Agent budgets: [`Budget`], [`budget_guards`] and usage sanitising.
//!
//! Budgets are GUARDS on the single way into a model turn (`checking_budget`), so no path
//! reaches the model over budget. The amounts they compare come from provider-reported usage,
//! which is untrusted input: [`spent_tokens`] / [`spent_usd`] clamp it so a hostile or buggy
//! provider can neither refund a budget (negative counts) nor disable it (NaN).

use serde_json::{Value, json};
use xstate_core::{DynContext, MachineLogic};

use crate::error::{AgentError, Result};

/// A usage number past this is treated as "budget exhausted" (fail closed).
pub const EXHAUSTED: f64 = 1e15;

/// Per-agent limits. `None` means "no limit on this axis".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Input + output tokens.
    pub max_tokens: Option<u64>,
    /// Cost in US dollars.
    pub max_usd: Option<f64>,
    /// Model turns (default 10).
    pub max_turns: Option<u64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_tokens: None, max_usd: None, max_turns: Some(10) }
    }
}

impl Budget {
    /// A validated budget.
    ///
    /// # Errors
    /// [`AgentError::Config`] when `max_usd` is negative or not finite.
    pub fn new(
        max_tokens: Option<u64>,
        max_usd: Option<f64>,
        max_turns: Option<u64>,
    ) -> Result<Self> {
        if let Some(usd) = max_usd
            && (!usd.is_finite() || usd < 0.0)
        {
            return Err(AgentError::config(format!(
                "Budget.max_usd must be a non-negative number or None (got {})",
                crate::pyfmt::py_float(usd)
            )));
        }
        Ok(Self { max_tokens, max_usd, max_turns })
    }

    /// Unlimited on every axis (no turn limit either).
    #[must_use]
    pub const fn unlimited() -> Self {
        Self { max_tokens: None, max_usd: None, max_turns: None }
    }

    /// Coerce `{"max_tokens", "max_usd", "max_turns"}` (missing keys keep their default,
    /// `null` means no limit), as Python's `Budget.coerce`.
    ///
    /// # Errors
    /// [`AgentError::Config`] for unknown keys or values of the wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Some(map) = value.as_object() else {
            return if value.is_null() {
                Ok(Self::default())
            } else {
                Err(AgentError::config("invalid budgets: not a mapping"))
            };
        };
        let mut out = Self::default();
        for (k, v) in map {
            match k.as_str() {
                "max_tokens" => out.max_tokens = limit_int(k, v)?,
                "max_turns" => out.max_turns = limit_int(k, v)?,
                "max_usd" if v.is_null() => out.max_usd = None,
                "max_usd" => {
                    out.max_usd = Some(
                        v.as_f64()
                            .filter(|_| !v.is_boolean())
                            .ok_or_else(|| bad(k, v, "number"))?,
                    );
                }
                other => {
                    return Err(AgentError::config(format!(
                        "invalid budgets: unexpected keyword argument '{other}'"
                    )));
                }
            }
        }
        Self::new(out.max_tokens, out.max_usd, out.max_turns)
    }
}

fn bad(name: &str, v: &Value, kind: &str) -> AgentError {
    AgentError::config(format!("Budget.{name} must be a non-negative {kind} or None (got {v})"))
}

fn limit_int(name: &str, v: &Value) -> Result<Option<u64>> {
    if v.is_null() {
        return Ok(None);
    }
    v.as_u64().map(Some).ok_or_else(|| bad(name, v, "int"))
}

/// `int(ctx.get(key, 0))` for a JSON context.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // clamped first
pub(crate) fn ctx_int(ctx: &Value, key: &str) -> i64 {
    match ctx.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0).clamp(-EXHAUSTED, EXHAUSTED) as i64),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

/// `float(ctx.get(key, 0.0))` for a JSON context.
#[must_use]
pub(crate) fn ctx_float(ctx: &Value, key: &str) -> f64 {
    match ctx.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => 0.0,
    }
}

fn sanitise(value: &Value) -> f64 {
    let v = match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.is_empty() => 0.0,
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        _ => f64::NAN,
    };
    let v = if !v.is_finite() || v > EXHAUSTED { EXHAUSTED } else { v };
    v.max(0.0)
}

/// A provider-reported token count: negative → 0, garbage / NaN / inf → budget-exhausting.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // within [0, EXHAUSTED]
pub fn spent_tokens(value: &Value) -> i64 {
    sanitise(value).trunc() as i64
}

/// A provider-reported cost: negative → 0, garbage / NaN / inf → budget-exhausting.
#[must_use]
pub fn spent_usd(value: &Value) -> f64 {
    sanitise(value)
}

/// `{"turns", "input_tokens", "output_tokens", "cost_usd"}` from an agent context.
#[must_use]
pub fn usage_totals(ctx: &Value) -> Value {
    json!({
        "turns": ctx_int(ctx, "turns"),
        "input_tokens": ctx_int(ctx, "tokens_in"),
        "output_tokens": ctx_int(ctx, "tokens_out"),
        "cost_usd": round10(ctx_float(ctx, "cost_usd")),
    })
}

fn round10(f: f64) -> f64 {
    (f * 1e10).round() / 1e10
}

/// Register `underTokenBudget` / `underCostBudget` / `underTurnLimit` on `logic`.
///
/// Each is `true` while the SPENT amount is strictly below the limit, so a turn is refused
/// once the limit is reached.
#[must_use]
pub fn budget_guards(logic: MachineLogic<DynContext>, budget: &Budget) -> MachineLogic<DynContext> {
    let Budget { max_tokens, max_usd, max_turns } = *budget;
    logic
        .guard("underTokenBudget", move |ctx, _, _| {
            Ok(max_tokens.is_none_or(|max| {
                let spent = ctx_int(ctx, "tokens_in").saturating_add(ctx_int(ctx, "tokens_out"));
                u64::try_from(spent).map_or(true, |s| s < max)
            }))
        })
        .guard("underCostBudget", move |ctx, _, _| {
            Ok(max_usd.is_none_or(|max| ctx_float(ctx, "cost_usd") < max))
        })
        .guard("underTurnLimit", move |ctx, _, _| {
            Ok(max_turns
                .is_none_or(|max| u64::try_from(ctx_int(ctx, "turns")).map_or(true, |t| t < max)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_validate_and_coerce() {
        assert!(Budget::new(None, Some(f64::NAN), None).is_err());
        assert!(Budget::new(None, Some(-1.0), None).is_err());
        let b = Budget::from_value(&json!({"max_turns": 3, "max_usd": 0.5}))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b, Budget { max_tokens: None, max_usd: Some(0.5), max_turns: Some(3) });
        assert_eq!(
            Budget::from_value(&Value::Null).unwrap_or_else(|e| panic!("{e}")),
            Budget::default()
        );
        assert!(Budget::from_value(&json!({"max_turns": "20"})).is_err());
        assert!(Budget::from_value(&json!({"max_turns": true})).is_err());
        assert!(Budget::from_value(&json!({"max_usd": true})).is_err());
        assert!(Budget::from_value(&json!({"nope": 1})).is_err());
        assert!(Budget::from_value(&json!([1])).is_err());
        assert_eq!(
            Budget::from_value(&json!({"max_turns": null}))
                .unwrap_or_else(|e| panic!("{e}"))
                .max_turns,
            None
        );
        assert_eq!(Budget::unlimited().max_turns, None);
    }

    #[test]
    fn spent_amounts_fail_closed() {
        assert_eq!(spent_tokens(&json!(-5)), 0);
        assert_eq!(spent_tokens(&json!("nan")), 1_000_000_000_000_000);
        assert_eq!(spent_tokens(&json!(7.9)), 7);
        assert!((spent_usd(&json!({})) - EXHAUSTED).abs() < 1.0);
        assert!(spent_usd(&Value::Null).abs() < f64::EPSILON);
    }

    #[test]
    fn totals_read_the_context() {
        let ctx = json!({"turns": 2, "tokens_in": 20, "tokens_out": 10, "cost_usd": 0.1 + 0.2});
        assert_eq!(
            usage_totals(&ctx),
            json!({"turns": 2, "input_tokens": 20, "output_tokens": 10, "cost_usd": 0.3})
        );
        assert_eq!(ctx_int(&json!({"x": 2.7}), "x"), 2);
        assert!((ctx_float(&json!({"x": true}), "x") - 1.0).abs() < f64::EPSILON);
    }
}
