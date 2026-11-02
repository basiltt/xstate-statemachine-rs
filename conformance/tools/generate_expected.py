#!/usr/bin/env python
"""Generate (or verify) the `expected` section of every conformance case.

Two oracles are consulted:

* **xstate** — the real XState v5 engine under Node (``tools/xstate_oracle/oracle.mjs``).
  This is the semantic oracle the Rust engine must match (ADR-0001).
* **python** — the Python ``xstate-statemachine`` library. Its transition observations are
  recorded for comparison, and its ``machine_hash`` and end-of-run snapshot are the
  byte-compatibility vectors for the Rust snapshot format (ADR-0003). The snapshot is
  recorded both parsed (``snapshot``) and as the exact ``get_snapshot()`` text
  (``snapshot_text``, volatile fields zeroed).

Where the two oracles disagree the case must carry a ``divergence`` object explaining why;
otherwise the run fails. ``divergences.generated.md`` is rewritten with the current list.

Usage::

    python conformance/tools/generate_expected.py           # rewrite expectations in place
    python conformance/tools/generate_expected.py --check   # fail if anything is stale

Run from the repository root. Requires Node >= 20 with ``npm ci`` done in
``tools/xstate_oracle`` and the Python library importable (``pip install xstate-statemachine``
or the library's own virtualenv).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CONFORMANCE = HERE.parent
CASES_DIR = CONFORMANCE / "cases"
ORACLE_JS = HERE / "xstate_oracle" / "oracle.mjs"
DIVERGENCES_MD = CONFORMANCE / "divergences.generated.md"

OBSERVATION_KEYS = ("state_ids", "context", "actions", "status")

# Volatile wall-clock fields. They are masked in the *text* of the snapshot (not only in the
# parsed object) so that the recorded `snapshot_text` is reproducible and a Rust test can
# compare text with text.
_VOLATILE_NUMBER = re.compile(r'("(?:taken_at|due_at_wall)": )[-+0-9.eE]+|("(?:taken_at|due_at_wall)": )null')


# --------------------------------------------------------------------------- oracles


def run_xstate_oracle(case_files: list[Path]) -> dict[str, Any]:
    cmd = ["node", str(ORACLE_JS), *map(str, case_files)]
    proc = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", check=False)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"xstate oracle failed with exit code {proc.returncode}")
    return json.loads(proc.stdout)


def run_python_oracle(case: dict[str, Any]) -> dict[str, Any]:
    import logging

    import xstate_statemachine as xsm
    from xstate_statemachine import SimulatedClock, SyncInterpreter, create_machine
    from xstate_statemachine.persistence.snapshot import structure_hash
    from xstate_statemachine.testing_utils import stub_logic

    # Failing stub services are expected in error-path cases; keep the engine's log quiet.
    logging.getLogger("xstate_statemachine").setLevel(logging.CRITICAL)

    cfg = json.loads(json.dumps(case["machine"]))  # private deep copy
    logic_spec = case.get("logic", {})
    ran: list[str] = []

    service_results = {
        name: spec.get("result")
        for name, spec in logic_spec.get("services", {}).items()
        if "error" not in spec
    }
    logic = stub_logic(cfg, ran=ran, guards=dict(logic_spec.get("guards", {})),
                       service_results=service_results)

    for name, spec in logic_spec.get("services", {}).items():
        if "error" in spec:
            message = str(spec["error"])

            def failing(*_args: Any, _m: str = message, **_kw: Any) -> Any:
                raise RuntimeError(_m)

            logic.services[name] = failing  # type: ignore[index]
    for name, ms in logic_spec.get("delays", {}).items():
        logic.delays[name] = ms  # type: ignore[index]

    machine = create_machine(cfg, logic=logic)
    clock = SimulatedClock()
    interp = SyncInterpreter(machine, clock=clock)

    def observe() -> dict[str, Any]:
        out: dict[str, Any] = {
            "state_ids": sorted(interp.current_state_ids),
            "context": json.loads(json.dumps(interp.context)),
            "actions": ran[:],
            "status": interp.status,
        }
        del ran[:]
        if interp.status == "done":
            out["output"] = getattr(interp, "output", None)
        if interp.status == "error":
            out["error"] = str(getattr(interp, "last_error", "") or "")
        return out

    interp.start()
    initial = observe()
    steps = []
    for step in case.get("steps", []):
        if "send" in step:
            ev = step["send"]
            if isinstance(ev, str):
                interp.send(ev)
            else:
                payload = {k: v for k, v in ev.items() if k != "type"}
                interp.send(ev["type"], **payload)
        elif "advance_ms" in step:
            clock.increment(step["advance_ms"])
        else:
            raise SystemExit(f"{case['name']}: unknown step {step!r}")
        steps.append(observe())

    # The exact text `get_snapshot()` returned (indent=2, ASCII-escaped), with the volatile
    # wall-clock fields zeroed so the fixture is reproducible.
    snapshot_text = _VOLATILE_NUMBER.sub(
        lambda m: (m.group(1) or m.group(2)) + "0.0", interp.get_snapshot()
    )
    snapshot = json.loads(snapshot_text)
    if snapshot.get("taken_at") != 0.0 or any(
        d.get("due_at_wall") != 0.0 for d in snapshot.get("deadlines", [])
    ):
        raise SystemExit(f"{case['name']}: volatile snapshot fields were not masked in the text")

    return {
        "oracle": {"name": "xstate-statemachine", "version": xsm.__version__},
        "initial": initial,
        "steps": steps,
        "machine_hash": structure_hash(machine),
        "snapshot": snapshot,
        "snapshot_text": snapshot_text,
    }


# --------------------------------------------------------------------------- comparison


def observation_diff(a: dict[str, Any], b: dict[str, Any]) -> list[str]:
    out = []
    for key in OBSERVATION_KEYS:
        if a.get(key) != b.get(key):
            out.append(f"{key}: xstate={a.get(key)!r} python={b.get(key)!r}")
    return out


def compare(xs: dict[str, Any], py: dict[str, Any]) -> list[str]:
    diffs = [f"initial: {d}" for d in observation_diff(xs["initial"], py["initial"])]
    for i, (sx, sp) in enumerate(zip(xs["steps"], py["steps"], strict=True)):
        diffs.extend(f"step {i}: {d}" for d in observation_diff(sx, sp))
    return diffs


# --------------------------------------------------------------------------- main


def strip_expected(case: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in case.items() if k != "expected"}


def write_divergences(rows: list[tuple[str, dict[str, Any] | None, list[str]]]) -> str:
    lines = [
        "# Oracle divergences (generated)",
        "",
        "Cases where the Python engine (`xstate-statemachine`) and XState v5 disagree.",
        "Regenerated by `conformance/tools/generate_expected.py`; do not edit by hand.",
        "The Rust engine follows XState (ADR-0001).",
        "",
    ]
    if not rows:
        lines.append("No divergences.")
    for name, note, diffs in rows:
        lines.append(f"## `{name}`")
        lines.append("")
        if note:
            lines.append(f"**Python behaviour:** {note.get('python', '')}")
            lines.append("")
            lines.append(f"**Spec / XState:** {note.get('spec', '')}")
            lines.append("")
        lines.append("```")
        lines.extend(diffs)
        lines.append("```")
        lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--check", action="store_true", help="verify instead of rewriting")
    parser.add_argument("--only", help="run a single case by name")
    args = parser.parse_args()

    case_files = sorted(CASES_DIR.glob("*.json"))
    cases = {f: json.loads(f.read_text(encoding="utf-8")) for f in case_files}
    if args.only:
        cases = {f: c for f, c in cases.items() if c["name"] == args.only}
        case_files = list(cases)

    names = [c["name"] for c in cases.values()]
    if len(set(names)) != len(names):
        raise SystemExit("duplicate case names")

    xs_all = run_xstate_oracle(case_files)
    stale: list[str] = []
    undocumented: list[str] = []
    rows: list[tuple[str, dict[str, Any] | None, list[str]]] = []

    for path, case in cases.items():
        name = case["name"]
        xs = xs_all["cases"].get(name)
        if xs is None or "error" in xs:
            raise SystemExit(f"{name}: xstate oracle error: {xs and xs.get('error')}")
        xs["oracle"] = xs_all["oracle"]
        try:
            py = run_python_oracle(case)
        except Exception as exc:  # noqa: BLE001 - the Python library refusing a chart is itself a finding
            import xstate_statemachine as xsm

            py = {
                "oracle": {"name": "xstate-statemachine", "version": xsm.__version__},
                "unsupported": f"{type(exc).__name__}: {exc}",
            }

        diffs = [f"python: {py['unsupported']}"] if "unsupported" in py else compare(xs, py)
        if diffs:
            rows.append((name, case.get("divergence"), diffs))
            if not case.get("divergence"):
                undocumented.append(name)
        elif case.get("divergence"):
            sys.stderr.write(f"note: {name} documents a divergence that no longer occurs\n")

        new_case = strip_expected(case)
        new_case["expected"] = {"xstate": xs, "python": py}
        text = json.dumps(new_case, indent=2, ensure_ascii=False) + "\n"
        if path.read_text(encoding="utf-8") != text:
            stale.append(path.name)
            if not args.check:
                path.write_text(text, encoding="utf-8", newline="\n")

    md = write_divergences(rows)
    if not DIVERGENCES_MD.exists() or DIVERGENCES_MD.read_text(encoding="utf-8") != md:
        stale.append(DIVERGENCES_MD.name)
        if not args.check:
            DIVERGENCES_MD.write_text(md, encoding="utf-8", newline="\n")

    for name in undocumented:
        sys.stderr.write(f"ERROR: {name}: oracles disagree but the case has no `divergence` note\n")
    if args.check and stale:
        sys.stderr.write("stale fixtures: " + ", ".join(stale) + "\n")
    print(f"{len(cases)} cases, {len(rows)} divergent, {len(stale)} {'stale' if args.check else 'written'}")
    return 1 if undocumented or (args.check and stale) else 0


if __name__ == "__main__":
    raise SystemExit(main())
