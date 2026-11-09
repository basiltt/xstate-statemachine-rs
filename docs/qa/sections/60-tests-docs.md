# §6.0 Tests and documentation claims

Scope: the §6.0 bullets "Review the tests themselves" and "Review the documentation claims".
Commit history, performance passes and alignment are separate sections.

## Golden provenance — verified

Every golden fixture is produced by an external oracle, not by the Rust code under test:

| Generator | Oracle | What it writes |
|---|---|---|
| `conformance/tools/generate_expected.py` | Node `xstate` 5.33.3 **and** Python 0.12.1 | the two observations in all 53 `conformance/cases/*.json` |
| `conformance/tools/export_golden.py` | Python 0.12.1 | `conformance/golden/{mermaid,plantuml}` (53 + 53 files) |
| `conformance/tools/inspect_golden.py` | Python 0.12.1 | inspector recordings |
| `conformance/tools/eda_golden.py` | Python 0.12.1 | envelope/dead-letter/AsyncAPI JSON and two SQLite databases |
| `conformance/tools/patterns_golden.py` | Python 0.12.1 | retry and circuit-breaker goldens |
| `conformance/tools/agents_golden.py` | Python 0.12.1 | six agent scenarios |

Checks run on 2026-11-07 with the venv's Python 0.12.1:

- `python -I conformance/tools/generate_expected.py --check` → `53 cases, 0 divergent, 0 stale`.
- `python -I conformance/tools/export_golden.py --check` → 53 `.mmd`, 53 `.puml`, no missing.
- `inspect_golden.py`, `eda_golden.py`, `patterns_golden.py`, `agents_golden.py` were re-run:
  no content change. The agent goldens came back CRLF-only on Windows (see L1 below); the EDA
  SQLite files differ at the page level (WAL vs delete-mode layout) while the Rust tests assert
  their exact rows and pass against the committed bytes.

The snapshot vectors in the conformance cases carry Python's snapshot text and structure hash
(`snapshot.rs` tests), so those are oracle-backed too.

## Test quality

- **Differential, not self-golden.** The fast path is compared with the general microstep by the
  same inputs (`engine/fast_path_tests.rs::fast_path_matches_general_path`, proptest; the 100 000
  case run took 334 s and passed), and the direct compiler is compared with the parser path on
  the full fingerprint including error strings (`machine/direct_tests.rs`). Both are
  implementation-vs-implementation, but the conformance suite is the external check on top.
- **Mutation sensitivity** was probed for the fast path (mutations B/C/D fail the differential
  in < 0.3 s). The 17-machine adversarial matrix and the 34 adversarial direct-compiler documents
  live only in the QA scratch copy (`docs/qa/work/audit-core`, finding L3 of
  `sections/60-perf-paths.md`); they are not in CI yet.
- **No test was found that accepts its own output as truth.** The export goldens, inspector
  recordings, EDA/patterns/agents goldens all originate in Python (table above).

## Findings

### HIGH-2 (carried): README performance table uses a retracted Python baseline

**Status: FIXED in `81af4c1`.** Re-measured with the Python 0.12.1 venv and the Rust benchmarks
on the same quiet machine; README, `benches/README.md` and `benches/budgets.json` carry the new
per-operation values and the qualified speed-up claim (S1/S3/S4/S6 past 100x, S2 71x, S5 28x,
S7 56x). See `sections/60-commits.md` HIGH-2.

`README.md:142-177` and `benches/budgets.json` quote Python 0.12.0 numbers from
`benchmarks/competitors/results.json` and state that S1/S3/S4 are "past 100x". The Python project
re-measured and retracted those numbers for 0.12.0/0.12.1 (see `sections/60-commits.md` HIGH-2).
The claim must be re-measured with the 0.12.1 venv on a quiet machine, and `budgets.json`,
the README table and `CHANGELOG.md` updated from the measurement. **Not fixed yet** — the
measurement needs an idle machine (the QA battle tests are using it); do not release with the
current wording.

### LOW-1: agent golden generator wrote platform line endings (fixed in `118374c`)

`conformance/tools/agents_golden.py:169` used `path.write_text(..., encoding="utf-8")` without
`newline="\n"`, so a regeneration on Windows left six CRLF files in the working tree (git
normalises them, so no commit damage). Reproduced by running the generator and observing
`git status`; fixed by passing `newline="\n"` like the other generators. A rerun now leaves the
tree clean.

## Not covered here

- Guide prose (`docs/guide/*.md`) was not line-by-line verified against the code in this pass;
  the docs-site build (§6.4) will exercise examples and links.
- The README feature matrix and benchmarks were not re-run; they depend on the quiet-machine
  measurement above.
