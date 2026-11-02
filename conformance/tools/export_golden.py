#!/usr/bin/env python
"""Generate (or verify) the Mermaid / PlantUML golden files of the Rust exporters.

``crates/xstate-core/tests/export.rs`` compares ``Machine::to_mermaid`` / ``to_plantuml`` with
the output of the Python library for every conformance machine:

    create_machine(cfg, logic=stub_logic(cfg)).to_mermaid()      ->  <case>.mmd
    create_machine(cfg, logic=stub_logic(cfg)).to_plantuml()     ->  <case>.puml

A case the Python library cannot build or render gets no golden file; the cases that are
expected to be missing are listed in the Rust test (``NO_MERMAID`` / ``NO_PLANTUML``) and the
generator prints them, so a change in what Python supports is a visible diff.

Usage::

    python conformance/tools/export_golden.py           # rewrite the golden files
    python conformance/tools/export_golden.py --check   # fail if any file is stale or stray

Run from the repository root with the Python library importable (``pip install
xstate-statemachine`` or the library's own virtualenv). No Node needed.
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
CASES_DIR = REPO / "conformance" / "cases"
GOLDEN_DIR = REPO / "crates" / "xstate-core" / "tests" / "fixtures" / "export"

RENDERERS = (("mmd", "to_mermaid"), ("puml", "to_plantuml"))


def render_all(cfg: dict) -> dict[str, str | None]:
    """Render one machine; an extension maps to ``None`` when Python cannot produce it."""
    from xstate_statemachine import create_machine
    from xstate_statemachine.testing_utils import stub_logic

    out: dict[str, str | None] = {}
    try:
        machine = create_machine(cfg, logic=stub_logic(cfg))
    except Exception:  # noqa: BLE001 - the library refusing a chart means no golden
        return {ext: None for ext, _ in RENDERERS}
    for ext, method in RENDERERS:
        try:
            out[ext] = getattr(machine, method)()
        except Exception:  # noqa: BLE001 - likewise for a renderer failure
            out[ext] = None
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--check", action="store_true", help="verify instead of rewriting")
    args = parser.parse_args()

    logging.getLogger("xstate_statemachine").setLevel(logging.CRITICAL)
    GOLDEN_DIR.mkdir(parents=True, exist_ok=True)

    wanted: dict[str, str] = {}
    missing: dict[str, list[str]] = {ext: [] for ext, _ in RENDERERS}
    for path in sorted(CASES_DIR.glob("*.json")):
        case = json.loads(path.read_text(encoding="utf-8"))
        rendered = render_all(json.loads(json.dumps(case["machine"])))
        for ext, _ in RENDERERS:
            text = rendered[ext]
            if text is None:
                missing[ext].append(path.stem)
            else:
                wanted[f"{path.stem}.{ext}"] = text

    stale: list[str] = []
    for name, text in wanted.items():
        target = GOLDEN_DIR / name
        current = target.read_text(encoding="utf-8") if target.exists() else None
        if current != text:
            stale.append(name)
            if not args.check:
                target.write_text(text, encoding="utf-8", newline="\n")

    stray = sorted(p.name for p in GOLDEN_DIR.iterdir() if p.is_file() and p.name not in wanted)
    for name in stray:
        stale.append(f"{name} (stray)")
        if not args.check:
            (GOLDEN_DIR / name).unlink()

    for ext, _ in RENDERERS:
        count = sum(1 for n in wanted if n.endswith(f".{ext}"))
        print(f".{ext}: {count} golden files; Python renders nothing for: {missing[ext] or 'none'}")
    if stale:
        sys.stderr.write(
            ("stale: " if args.check else "rewritten: ") + ", ".join(stale) + "\n"
        )
    return 1 if args.check and stale else 0


if __name__ == "__main__":
    raise SystemExit(main())
