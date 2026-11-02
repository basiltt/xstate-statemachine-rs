"""Throughput of ``send`` on a toggle machine: Rust engine (``xstate_rs``) vs the pure-Python
``xstate_statemachine.SyncInterpreter``.

Run from a virtualenv with both packages installed::

    python crates/xstate-py/bench/bench.py [events]

Two scenarios: a bare toggle (no callbacks: the Rust engine runs detached from Python) and a
toggle whose transitions call one Python action (every event crosses into Python once).
"""

from __future__ import annotations

import logging
import sys
import time
from typing import Any, Callable, Dict, Optional

import xstate_rs
from xstate_statemachine import MachineLogic, SyncInterpreter, create_machine


def toggle_config(with_action: bool) -> Dict[str, Any]:
    on: Any = {"target": "active", "actions": "count"} if with_action else "active"
    off: Any = {"target": "inactive", "actions": "count"} if with_action else "inactive"
    return {
        "id": "toggle",
        "initial": "inactive",
        "context": {"n": 0},
        "states": {
            "inactive": {"on": {"TOGGLE": on}},
            "active": {"on": {"TOGGLE": off}},
        },
    }


def rate(send: Callable[[str], Any], events: int) -> float:
    for _ in range(min(1000, events)):  # warm up
        send("TOGGLE")
    started = time.perf_counter()
    for _ in range(events):
        send("TOGGLE")
    return events / (time.perf_counter() - started)


def rust_send(with_action: bool) -> Callable[[str], Any]:
    machine = xstate_rs.Machine.from_dict(toggle_config(with_action))
    logic: Optional[xstate_rs.MachineLogic] = None
    if with_action:

        def count(ctx: Dict[str, Any], event: Dict[str, Any], params: Any) -> None:
            ctx["n"] += 1

        logic = xstate_rs.MachineLogic(actions={"count": count})
    interp = xstate_rs.SyncInterpreter(machine, logic)
    interp.start()
    return interp.send


def python_send(with_action: bool) -> Callable[[str], Any]:
    def count(interp: Any, ctx: Dict[str, Any], event: Any, action: Any) -> None:
        ctx["n"] += 1

    logic: Optional[MachineLogic[Any]] = MachineLogic(actions={"count": count}) if with_action else None
    machine = create_machine(toggle_config(with_action), logic=logic)
    interp = SyncInterpreter(machine).start()
    return interp.send


def main() -> None:
    logging.getLogger("xstate_statemachine").setLevel(logging.CRITICAL)
    events = int(sys.argv[1]) if len(sys.argv) > 1 else 200_000
    print(f"send() throughput, {events:,} events, Python {sys.version.split()[0]}")
    for with_action in (False, True):
        label = "toggle + 1 Python action" if with_action else "bare toggle"
        rust = rate(rust_send(with_action), events)
        python = rate(python_send(with_action), max(events // 20, 1000))
        print(f"  {label:<26} xstate_rs: {rust:>12,.0f} ev/s   "
              f"xstate_statemachine: {python:>10,.0f} ev/s   ({rust / python:,.1f}x)")


if __name__ == "__main__":
    main()
