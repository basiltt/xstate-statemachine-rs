//! The circuit-breaker chart and the logic it names.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};
use xstate_core::{ActionScope, DynContext, MachineLogic};

use crate::ctx::{counter, set_key};

/// The breaker chart. `cooldown` is a *named* delay so the same JSON serves every breaker;
/// `failure_threshold` and `half_open_max_calls` live in context so guards read them. Feed it
/// to any inspector as-is. Identical to the Python library's `CIRCUIT_BREAKER_CONFIG`.
pub static CIRCUIT_BREAKER_CONFIG: LazyLock<Value> = LazyLock::new(build_config);

/// An owned copy of [`CIRCUIT_BREAKER_CONFIG`].
#[must_use]
pub fn circuit_breaker_config() -> Value {
    CIRCUIT_BREAKER_CONFIG.clone()
}

fn build_config() -> Value {
    json!({
        "id": "circuitBreaker",
        "version": "1",
        "initial": "closed",
        "context": {
            "failures": 0,
            "failure_threshold": 5,
            "half_open_calls": 0,
            "half_open_max_calls": 1,
            "opened_count": 0
        },
        "states": {
            "closed": {
                "description": "Calls pass through; consecutive failures counted.",
                "entry": "resetFailures",
                "on": {
                    "SUCCESS": { "actions": "resetFailures" },
                    "FAILURE": [
                        {
                            "guard": "thresholdReached",
                            "target": "open",
                            "actions": "countFailure"
                        },
                        { "actions": "countFailure" }
                    ]
                }
            },
            "open": {
                "description": "Calls fail fast; waiting out the cooldown.",
                "tags": ["rejecting"],
                "entry": "countOpen",
                "after": { "cooldown": { "target": "half_open" } }
            },
            "half_open": {
                "description": "A bounded number of probe calls is admitted.",
                "entry": "resetProbes",
                "on": {
                    "PROBE": { "guard": "probeAvailable", "actions": "countProbe" },
                    "SUCCESS": { "target": "closed" },
                    "FAILURE": { "target": "open" }
                }
            }
        }
    })
}

type CounterAction =
    Box<dyn Fn(&mut ActionScope<'_, DynContext>) -> xstate_core::Result<()> + Send + Sync>;

/// An action adding one to the counter at `key`.
fn bump(key: &'static str) -> CounterAction {
    Box::new(move |scope| {
        let next = counter(scope.context, key) + 1;
        set_key(scope.context, key, Value::from(next));
        Ok(())
    })
}

/// An action setting the counter at `key` to zero.
fn zero(key: &'static str) -> CounterAction {
    Box::new(move |scope| {
        set_key(scope.context, key, Value::from(0));
        Ok(())
    })
}

/// The logic [`CIRCUIT_BREAKER_CONFIG`] needs, with the cooldown bound to `cooldown_ms`.
#[must_use]
pub fn circuit_breaker_logic(cooldown_ms: f64) -> MachineLogic<DynContext> {
    breaker_logic(cooldown_ms, &Arc::new(AtomicU64::new(0)))
}

/// [`circuit_breaker_logic`] whose `probeAvailable` guard also honours `released`: probe slots
/// given back (a probe that ended in a way the breaker does not count) in the current
/// half-open window. The chart is untouched; the count lives outside the context because a
/// release happens outside any transition. `resetProbes` zeroes it on every half-open entry.
pub(super) fn breaker_logic(
    cooldown_ms: f64,
    released: &Arc<AtomicU64>,
) -> MachineLogic<DynContext> {
    let (zero_released, guard_released) = (Arc::clone(released), Arc::clone(released));
    MachineLogic::dynamic()
        .action("countFailure", bump("failures"))
        .action("resetFailures", zero("failures"))
        .action("countOpen", bump("opened_count"))
        .action("resetProbes", move |scope: &mut ActionScope<'_, DynContext>| {
            set_key(scope.context, "half_open_calls", Value::from(0));
            zero_released.store(0, Ordering::Relaxed);
            Ok(())
        })
        .action("countProbe", bump("half_open_calls"))
        // +1: this FAILURE is being counted by the same transition.
        .guard("thresholdReached", |ctx, _event, _params| {
            Ok(counter(ctx, "failures") + 1 >= counter(ctx, "failure_threshold"))
        })
        .guard("probeAvailable", move |ctx, _event, _params| {
            let in_flight = counter(ctx, "half_open_calls")
                .saturating_sub(guard_released.load(Ordering::Relaxed));
            Ok(in_flight < counter(ctx, "half_open_max_calls"))
        })
        .delay("cooldown", cooldown_ms)
}
