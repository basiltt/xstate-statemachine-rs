#!/usr/bin/env python
"""Regenerate the xstate-patterns golden fixtures from the Python library.

Writes into ``crates/xstate-patterns/tests/golden``:

* ``saga_plain.json``, ``saga_retry.json``, ``saga_timeouts.json`` -- the output of
  ``SagaBuilder.build()`` for three sagas (the Rust tests rebuild the same sagas and compare
  ``serde_json::Value`` equality);
* ``circuit_breaker_config.json`` -- ``CIRCUIT_BREAKER_CONFIG``;
* ``retry_delays.json`` -- ``RetryPolicy.delay_ms`` for every jitter mode with a constant rng.

Usage: ``python -I conformance/tools/patterns_golden.py`` with the Python library importable.
"""

from __future__ import annotations

import json
from pathlib import Path

GOLDEN = Path(__file__).resolve().parents[2] / "crates" / "xstate-patterns" / "tests" / "golden"
RNG_SAMPLE = 0.25
RETRY_PARAMS = {"max_attempts": 6, "base_ms": 100.0, "factor": 2.0, "max_ms": 1500.0}


def saga_plain():
    from xstate_statemachine.patterns import SagaBuilder

    return (
        SagaBuilder("order_plain")
        .step("reserve", invoke="reserveStock")
        .step("charge", invoke="chargeCard")
        .step("ship", invoke="shipOrder")
        .on_failure("notifyOps")
    )


def saga_retry():
    from xstate_statemachine.patterns import RetryPolicy, SagaBuilder

    policy = RetryPolicy(max_attempts=3, base_ms=100.0, factor=2.0, max_ms=1000.0, jitter="none")
    return (
        SagaBuilder("order_retry")
        .step(
            "reserve_stock",
            invoke="reserveStock",
            compensate="releaseStock",
            retry=policy,
        )
        .step("charge_card", invoke="chargeCard", compensate="refundCard")
        .step("ship", invoke="shipOrder", retry=policy)
    )


def saga_timeouts():
    from xstate_statemachine.patterns import RetryPolicy, SagaBuilder

    return (
        SagaBuilder("order_timeouts", start_event="ORDER_PLACED")
        .step("reserve_stock", invoke="reserveStock", compensate="releaseStock", timeout_ms=5000)
        .step("verify", invoke="verifyFraud", timeout_ms=2500)
        .step(
            "charge_card",
            invoke="chargeCard",
            compensate="refundCard",
            timeout_ms=10000,
            retry=RetryPolicy(max_attempts=2, jitter="none"),
        )
        .step("ship", invoke="shipOrder")
        .on_failure("notifyOps", "pageOncall")
    )


def retry_delays():
    from xstate_statemachine.patterns import RetryPolicy

    out = {}
    for mode in ("none", "full", "equal", "decorrelated"):
        policy = RetryPolicy(jitter=mode, rng=lambda: RNG_SAMPLE, **RETRY_PARAMS)
        delays = []
        previous = None
        for attempt in range(1, RETRY_PARAMS["max_attempts"] + 1):
            ms = policy.delay_ms(attempt, previous_ms=previous)
            delays.append(ms)
            previous = ms
        out[mode] = delays
    return {"params": RETRY_PARAMS, "rng": RNG_SAMPLE, "delays": out}


def write(name: str, data) -> None:
    GOLDEN.mkdir(parents=True, exist_ok=True)
    path = GOLDEN / f"{name}.json"
    text = json.dumps(data, indent=2, ensure_ascii=True) + "\n"
    path.write_text(text, encoding="utf-8", newline="\n")
    print(f"{path.name}: {len(text)} bytes")


def main() -> int:
    from xstate_statemachine.patterns import CIRCUIT_BREAKER_CONFIG

    write("saga_plain", saga_plain().build())
    write("saga_retry", saga_retry().build())
    write("saga_timeouts", saga_timeouts().build())
    write("circuit_breaker_config", CIRCUIT_BREAKER_CONFIG)
    write("retry_delays", retry_delays())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
