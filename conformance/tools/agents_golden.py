#!/usr/bin/env python
"""Regenerate the xstate-agents golden fixtures from the Python library.

Writes one JSON file per scenario into ``crates/xstate-agents/tests/golden``:

* ``plain_answer.json`` -- the model answers at once;
* ``tool_then_answer.json`` -- one read-only tool call (its output carries a secret that
  ``scrub`` must mask), then the answer;
* ``approval.json`` -- a ``side_effect`` tool parks the agent in ``awaiting_human``; a second
  ``run_agent_sync(approve=True)`` with the same store resumes it to ``done``;
* ``budget_exhausted.json`` -- ``max_turns=1`` refuses the second model turn;
* ``rejected.json`` -- the reviewer rejects the side-effect call (``approve=False``) and the
  model answers after seeing the rejection;
* ``denied_arguments.json`` -- schema-invalid arguments are denied by the ``toolAllowed``
  guard before any human is asked.

Each file holds, per ``run_agent_sync`` call (a "phase"): the ``AgentResult`` fields, the
sequence of leaf states after each transition, and the trace records of an
``AgentTracePlugin`` with ``record_content=True``. Volatile values are normalised: ``ts`` → 0,
interpreter ids → ``"<agent>"``, FakeModel call ids → ``call_<n>_<k>``.

Usage: ``python -I conformance/tools/agents_golden.py`` with the Python library (and
pydantic, the ``agents`` extra) importable.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

GOLDEN = Path(__file__).resolve().parents[2] / "crates" / "xstate-agents" / "tests" / "golden"
CALL_ID = re.compile(r"call_[0-9a-f]{8}_(\d+)_(\d+)")


def lookup(query: str) -> dict:
    """Look a fact up in the knowledge base."""
    return {"answer": 42, "query": query, "api_key": "k-123", "note": "Bearer abcdefghijklmnop"}


def refund(order_id: str, amount_cents: int) -> str:
    """Refund an order."""
    return f"refunded {amount_cents} on {order_id}"


def registry():
    from xstate_statemachine.contrib.agents import tool, tool_registry

    return tool_registry(lookup, tool(refund, side_effect=True, timeout_s=5.0))


class Targets:
    """Plugin recording the sorted leaf state ids after every transition taken."""

    def __init__(self):
        self.seen = []

    def __getattr__(self, name):
        if name.startswith("on_"):
            return lambda *a, **k: None
        raise AttributeError(name)

    def on_transition(self, interpreter, from_states, to_states, transition):
        self.seen.append(sorted(interpreter.current_state_ids))


def normalise(value, ids):
    if isinstance(value, dict):
        return {k: normalise(v, ids) for k, v in value.items()}
    if isinstance(value, list):
        return [normalise(v, ids) for v in value]
    if isinstance(value, str):
        if value in ids:
            return "<agent>"
        return CALL_ID.sub(lambda m: f"call_{m.group(1)}_{m.group(2)}", value)
    return value


def run(phases, model, tools, store, budgets):
    from xstate_statemachine.contrib.agents import AgentTracePlugin, run_agent_sync

    out = []
    for kwargs in phases:
        trace = AgentTracePlugin(record_content=True, clock=lambda: 0.0)
        targets = Targets()
        res = run_agent_sync(
            model=model,
            tools=tools,
            store=store,
            key="agent-1" if store is not None else None,
            plugins=[trace, targets],
            tracer=trace,
            budgets=budgets,
            **kwargs,
        )
        ids = {r["agent_id"] for r in trace.records} | {r["trace_id"] for r in trace.records}
        ids.discard(None)
        out.append(
            normalise(
                {
                    "input": kwargs,
                    "result": {
                        "final_state": res.final_state,
                        "status": res.status,
                        "context": res.context,
                        "output": res.output,
                        "error": res.error,
                        "waiting": res.waiting,
                        "usage": res.usage,
                    },
                    "states": targets.seen,
                    "trace": trace.records,
                    "totals": trace.totals(),
                },
                ids,
            )
        )
    return out


def scenario(script, phases, *, store=False, budgets=None):
    from xstate_statemachine.contrib.agents import FakeModel
    from xstate_statemachine.persistence import MemoryStore

    model = FakeModel(script, is_async=False)
    return {
        "script": script,
        "budgets": budgets,
        "phases": run(phases, model, registry(), MemoryStore() if store else None, budgets),
    }


def main():
    GOLDEN.mkdir(parents=True, exist_ok=True)
    scenarios = {
        "plain_answer": scenario([{"text": "hello there"}], [{"prompt": "say hi"}]),
        "tool_then_answer": scenario(
            [{"tool": "lookup", "args": {"query": "meaning"}}, {"text": "it is 42"}],
            [{"prompt": "what is the meaning?"}],
        ),
        "approval": scenario(
            [
                {"tool": "refund", "args": {"order_id": "A-1", "amount_cents": 500}},
                {"text": "refund issued", "usage": {"cost_usd": 0.25}},
            ],
            [{"prompt": "refund order A-1"}, {"approve": True}],
            store=True,
        ),
        "budget_exhausted": scenario(
            [{"tool": "lookup", "args": {"query": "q"}}, {"text": "never reached"}],
            [{"prompt": "look it up"}],
            budgets={"max_turns": 1},
        ),
        "rejected": scenario(
            [
                {"tool": "refund", "args": {"order_id": "B-2", "amount_cents": 900}},
                {"text": "understood, no refund"},
            ],
            [{"prompt": "refund order B-2"}, {"approve": False}],
            store=True,
        ),
        "denied_arguments": scenario(
            [{"tool": "refund", "args": {"order_id": "C-3", "amount_cents": "all"}}],
            [{"prompt": "refund everything"}],
        ),
    }
    for name, data in scenarios.items():
        path = GOLDEN / f"{name}.json"
        path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
