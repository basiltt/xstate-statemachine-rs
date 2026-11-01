#!/usr/bin/env python
"""Regenerate the inspector golden recordings from the Python library.

Each case in ``crates/xstate-inspect/tests/golden/cases.json`` is run through the Python
``InspectorPlugin`` with a deterministic clock and written as ``<name>.jsonl`` in the same
folder, in ``JsonLinesSink`` format. ``tests/golden.rs`` replays the same cases through the
Rust ``InspectorObserver`` and compares the lines byte for byte.

Usage: ``python -I conformance/tools/inspect_golden.py`` with the Python library importable.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

GOLDEN = Path(__file__).resolve().parents[2] / "crates" / "xstate-inspect" / "tests" / "golden"
CLOCK_START = 1_700_000_000.0
CLOCK_STEP = 0.5


class Clock:
    def __init__(self) -> None:
        self.now = CLOCK_START - CLOCK_STEP

    def __call__(self) -> float:
        self.now += CLOCK_STEP
        return self.now


def run_case(case: dict) -> list[str]:
    from xstate_statemachine import SyncInterpreter, create_machine
    from xstate_statemachine.inspect import InspectorPlugin, MemorySink
    from xstate_statemachine.testing_utils import stub_logic

    cfg = case["config"]
    logic = stub_logic(cfg, ran=[])
    machine = create_machine(cfg, logic=logic)
    sink = MemorySink()
    opts = dict(case.get("options", {}))
    plugin = InspectorPlugin(sink, clock=Clock(), **opts)
    interp = SyncInterpreter(machine).use(plugin)
    interp.start()
    for ev in case["events"]:
        interp.send(ev["type"], **ev.get("payload", {}))
    interp.stop()
    return [
        json.dumps(m, default=str, separators=(",", ":"), ensure_ascii=True)
        for m in sink.messages
    ]


def main() -> int:
    spec = json.loads((GOLDEN / "cases.json").read_text(encoding="utf-8"))
    for case in spec["cases"]:
        lines = run_case(case)
        out = GOLDEN / f"{case['name']}.jsonl"
        out.write_text("".join(line + "\n" for line in lines), encoding="utf-8", newline="\n")
        print(f"{out.name}: {len(lines)} messages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
