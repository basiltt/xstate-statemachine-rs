#!/usr/bin/env python3
"""Performance gate for the engine benchmarks (stdlib only).

Runs the criterion benchmarks of ``xstate-core`` (or reads the estimates of a previous run),
compares each benchmark's median against ``benches/budgets.json`` and prints a table that also
shows the Python library's per-operation time and the speed-up over it.

A benchmark fails when its median exceeds ``baseline_p50_ns * allowed_slowdown``. With
``--relative`` the limit is first scaled by the machine speed factor -- the median of
``measured / baseline`` over every measured row -- so a uniformly slower machine (a CI runner)
does not fail, while one benchmark regressing against the others still does.

Usage::

    python scripts/perf_gate.py                       # run the benches, gate, exit 1 on regression
    python scripts/perf_gate.py --no-run              # gate the estimates already in target/criterion
    python scripts/perf_gate.py --advisory            # print the table, never fail (CI runners)
    python scripts/perf_gate.py --update-baselines    # run, then write the medians as baselines
    python scripts/perf_gate.py -- --warm-up-time 1   # extra arguments go to criterion
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BUDGETS = ROOT / "benches" / "budgets.json"
DEFAULT_CRITERION = ROOT / "target" / "criterion"
DEFAULT_BENCHES = ("core", "python_parity")
PACKAGE = "xstate-core"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    parser.add_argument("--criterion-dir", type=Path, default=DEFAULT_CRITERION)
    parser.add_argument(
        "--bench",
        action="append",
        dest="benches",
        help=f"bench target to run (repeatable; default: {', '.join(DEFAULT_BENCHES)})",
    )
    parser.add_argument("--no-run", action="store_true", help="only read existing estimates")
    parser.add_argument(
        "--advisory", action="store_true", help="report regressions but exit 0 (noisy hardware)"
    )
    parser.add_argument(
        "--relative",
        action="store_true",
        help="scale the limits by the median measured/baseline ratio (machine speed factor)",
    )
    parser.add_argument(
        "--update-baselines",
        action="store_true",
        help="write the measured medians into budgets.json as the new baselines",
    )
    parser.add_argument("criterion_args", nargs="*", help="passed to criterion after `--`")
    return parser.parse_args(argv)


def run_benches(benches: list[str], extra: list[str]) -> None:
    cmd = ["cargo", "bench", "-p", PACKAGE]
    for name in benches:
        cmd += ["--bench", name]
    cmd += ["--", "--noplot", *extra]
    print("$", " ".join(cmd), flush=True)
    result = subprocess.run(cmd, cwd=ROOT, check=False)
    if result.returncode != 0:
        sys.exit(f"perf_gate: `cargo bench` failed with exit code {result.returncode}")


def read_median(criterion_dir: Path, bench_id: str) -> float | None:
    """The median point estimate (ns) of the latest run of `bench_id`, if there is one."""
    path = criterion_dir / bench_id / "new" / "estimates.json"
    try:
        estimates = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    median = estimates.get("median", {}).get("point_estimate")
    return float(median) if isinstance(median, (int, float)) else None


def fmt_ns(ns: float | None) -> str:
    if ns is None:
        return "-"
    if ns >= 1_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    if ns >= 1_000:
        return f"{ns / 1_000:.2f} us"
    return f"{ns:.0f} ns"


def speed_factor(rows: list[dict]) -> float:
    ratios = [r["p50"] / r["baseline"] for r in rows if r["p50"] and r["baseline"]]
    return statistics.median(ratios) if ratios else 1.0


def evaluate(budgets: dict, criterion_dir: Path, relative: bool) -> tuple[list[dict], float]:
    allowed = float(budgets["allowed_slowdown"])
    rows = []
    for bench_id, entry in budgets["benchmarks"].items():
        rows.append(
            {
                "id": bench_id,
                "p50": read_median(criterion_dir, bench_id),
                "baseline": entry.get("baseline_p50_ns"),
                "python": entry.get("python_per_op_ns"),
            }
        )
    factor = speed_factor(rows) if relative else 1.0
    for row in rows:
        if row["p50"] is None:
            row["limit"], row["status"] = None, "not run"
        elif row["baseline"] is None:
            row["limit"], row["status"] = None, "new"
        else:
            row["limit"] = row["baseline"] * allowed * factor
            row["status"] = "ok" if row["p50"] <= row["limit"] else "REGRESSION"
    return rows, factor


def print_table(rows: list[dict], factor: float, relative: bool) -> None:
    header = ("benchmark", "p50", "baseline", "limit", "status", "python/op", "speed-up", "100x")
    table = [header]
    for r in rows:
        python = r["python"]
        speedup = f"{python / r['p50']:.0f}x" if python and r["p50"] else "-"
        target = "met" if python and r["p50"] and python / r["p50"] >= 100 else "-"
        if python and r["p50"] and target == "-":
            target = f"{100 * r['p50'] / python:.1f}x off"
        table.append(
            (
                r["id"],
                fmt_ns(r["p50"]),
                fmt_ns(r["baseline"]),
                fmt_ns(r["limit"]),
                r["status"],
                fmt_ns(python),
                speedup,
                target,
            )
        )
    widths = [max(len(row[i]) for row in table) for i in range(len(header))]
    for i, row in enumerate(table):
        print("  ".join(cell.ljust(widths[j]) for j, cell in enumerate(row)).rstrip())
        if i == 0:
            print("  ".join("-" * w for w in widths))
    if relative:
        print(f"\nmachine speed factor (median measured/baseline): {factor:.2f}")


def update_baselines(path: Path, budgets: dict, rows: list[dict]) -> int:
    changed = 0
    for row in rows:
        if row["p50"] is None:
            continue
        budgets["benchmarks"][row["id"]]["baseline_p50_ns"] = round(row["p50"])
        changed += 1
    path.write_text(json.dumps(budgets, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return changed


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    budgets = json.loads(args.budgets.read_text(encoding="utf-8"))
    if not args.no_run:
        run_benches(args.benches or list(DEFAULT_BENCHES), args.criterion_args)
    rows, factor = evaluate(budgets, args.criterion_dir, args.relative)
    print_table(rows, factor, args.relative)
    if args.update_baselines:
        count = update_baselines(args.budgets, budgets, rows)
        print(f"\nperf_gate: wrote {count} baselines to {args.budgets}")
        return 0
    regressions = [r["id"] for r in rows if r["status"] == "REGRESSION"]
    if not regressions:
        print("\nperf_gate: no regression")
        return 0
    print(f"\nperf_gate: {len(regressions)} regression(s): {', '.join(regressions)}")
    if args.advisory:
        print("perf_gate: advisory mode, not failing")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
