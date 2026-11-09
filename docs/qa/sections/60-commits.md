# §6.0 Commit-by-commit review

Severity tally: CRITICAL 0 · HIGH 2 · MEDIUM 3 · LOW 2 (all open; none fixed by this
review — it is report-only).

Status update 2026-11-07: HIGH-1 is fixed by `477046c` + `32881c3` (fixtures regenerated
against Python 0.12.1 with zero behavioural diffs; CI pins the released 0.12.1 instead of
`git main`; stale README/CONTRIBUTING/handover claims corrected). HIGH-2 is in progress
(Python-side re-measurement on this machine). M1, M2, M3, L1, L2 remain open; M1's fix
design is being worked in `sections/60-perf-paths.md`.

## Method and evidence boundary

All 40 commits in `git log --reverse` (from `a3f0f84` to `2850cf8`, clean tree at the
start of the review apart from the untracked `docs/qa/`) were checked for:

- subject/body against the changed files and insertion/deletion counts;
- whether behaviour commits added tests in the same commit;
- unrelated files, CI weakening, `unsafe`/`todo!`/`dbg!`/`#[ignore]`, and golden-file
  provenance.

The full diff was read for `2850cf8`, `e194300`, `cf541e6` and `9ec75e0`'s changed
engine/machine/test files; targeted hunks or fixtures were read for `6caa8ab`,
`cfb0817`, `1abec8d`, `eed5e35`, `5efc80e`, `6c4e652`, `32ffe52` and `d247434`. The
remaining bulk is covered by the file/test inventory and the oracle checks below, not
by a line-by-line reading of all ~130k inserted lines.

Evidence commands were run at HEAD `2850cf8` unless stated. Logs are in
`docs/qa/work/`: `oracle_check_0121.log`, `oracle_regen_0121.log`,
`export_check_0121.log`, `core_lib_tests.log`, `conformance_tests.log`,
`proptest_fast_path_100k.log`.

## Findings

### HIGH-1: the conformance-fixtures CI job is red at HEAD (Python oracle drifted to 0.12.1)

- **Where:** `.github/workflows/ci.yml:22` (`XSM_PYTHON_REQ` installs the Python library
  from `git ...@main`), `.github/workflows/ci.yml:237-240` (installs it, then runs
  `generate_expected.py --check`); `conformance/cases/001_toggle_basic.json:79` records
  `"version": "0.12.0"` (all 53 cases do).
- **What:** Python `main` is now the released **0.12.1** (PyPI latest; local venv
  `xstate_statemachine.__version__ == "0.12.1"`; Python repo `main` is merge `57f6bc38`
  of the 0.12.1 release). The fixture checker embeds the Python oracle version in every
  expected section, so the job fails on the version field alone.
- **Repro:** with the documented venv,
  `python -I conformance/tools/generate_expected.py --check` →
  `53 cases, 0 divergent, 53 stale` (exit 1).
- **Is behaviour stale?** No. Regenerating all 53 cases in a throwaway copy of
  `conformance/` (junction to the existing `node_modules`) gives
  `53 cases, 0 divergent, 52 written`; a structural comparison with the oracle version
  fields masked found **0 cases with a non-version difference**. The Rust engine still
  matches Python 0.12.1 on every case; only the recorded label changed.
- **Related false claim:** `README.md:331` says CI uses `main` "because 0.12.0 is not on
  PyPI yet (PyPI has 0.11.0)". PyPI has 0.12.0 and 0.12.1; the statement is false. The
  same stale comment is in `ci.yml:19-22`.
- **Related stale QA documentation:** `docs/HANDOVER-QA.md:144-145` says the checker is
  "0 stale against the released Python 0.12.0 (the venv has it installed)", and
  `:244` repeats that the venv has 0.12.0; both are false in the current environment
  (0.12.1, 53 stale). `:326` and `:393` call the FileStore change "unreleased" and
  `CHANGELOG.md:287` still describes the port as 0.12.0 only; 0.12.1 shipped that
  FileStore change. Update these in the same commit as the pin/regeneration.
- **Fix (owner choice, no hand-editing):** pin `XSM_PYTHON_REQ` for the fixture and
  integration jobs to `xstate-statemachine[redis,sqlalchemy,nats,rabbitmq,sqs]==0.12.0`
  (or `==0.12.1`) and regenerate the fixtures with the generator against that exact
  version; update `ci.yml`'s comment, `README.md:331`, `CONTRIBUTING.md` and
  `HANDOVER-QA.md`. Regenerating to 0.12.1 is behaviour-safe on the current 53 cases and
  also records the environment the venv actually has. This is required before CI's first
  run can be green (release checklist §9 depends on CI).
- **Deferral:** none justified; it blocks the first green CI run.

### HIGH-2: the README performance table uses a Python baseline that the Python project retracted; "past 100x" is false

**Status: FIXED in `81af4c1`.** Both sides were re-measured on the same quiet machine with the
Python 0.12.1 venv and the Rust `python_parity`/`core` benchmarks. The retracted 0.12.0 numbers
are gone from README, `benches/README.md` and `benches/budgets.json`; the table now reports
Python 18.69 / 11.42 / 36.06 / 47.66 / 115 / 45.2 / 190 µs per operation and Rust 106 / 161 /
215 / 283 / 4.11 µs / 300 ns / 3.39 µs (criterion median). S1, S3, S4 and S6 are past 100x
(177x, 168x, 169x, 151x); S2 71x, S7 56x, S5 28x — stated as such instead of "past 100x"
without qualification. `perf_gate` reports no regression against the refreshed baselines.


- **Where:** `README.md:142-153` and `benches/budgets.json:65-102` pin
  `python_per_op_ns` S1 12,351.8; S2 10,537.0; S3 28,367.3; S4 37,863.1; S5 78,994.5;
  S6 21,910.8; S7 118,033.0, all labelled "Python 0.12.0 sync
  (`benchmarks/competitors/results.json`)".
- **What:** those are the **2026-11-05 low-sample table** numbers of the Python project
  (S1 80,960 ev/s, S2 94,903 ev/s, …). The Python 0.12.1 CHANGELOG and competitor
  README explicitly retract that sample: the corrected 0.12.0 quiet re-run is S1 96,369,
  S2 102,251, S3 39,178, S4 30,461 ev/s, and the released 0.12.1 table is S1 95,618,
  S2 164,764, S3 41,375, S4 30,972 ev/s. Rust's S2 176 ns no longer represents 60x.
- **Repro (arithmetic on reviewed files):** Rust pass S1 116 ns, S2 176 ns, S3 252 ns,
  S4 302 ns (`README.md:144-147`). Speed-ups:

| Scenario | README claim (old 0.12.0 sample) | vs corrected 0.12.0 | vs released 0.12.1 |
|---|---:|---:|---:|
| S1 | 106x | **89.5x** | **90.2x** |
| S2 | 60x | 55.6x | **34.5x** |
| S3 | 113x | 101.3x | **95.9x** |
| S4 | 125x | 108.7x | 106.9x |

- **Consequence:** `README.md:153` ("S1, S3 and S4 are past 100x") is false against
  both current datasets; against 0.12.1 only S4 is past 100x. Commit `9ec75e0`'s
  "S1 106x" and the 100x target column are therefore inflated, and `perf_gate.py` prints
  speed-ups against the same stale baselines. The CI bench job is advisory, so nothing
  catches this.
- **Fix:** re-measure the Python competitor table on the same host/day (the mandate's
  requirement) against 0.12.1, or adopt the released `results.json` values explicitly;
  update `benches/budgets.json`, `README.md`, `benches/README.md`,
  `docs/HANDOVER-QA.md` §3 and the CHANGELOG. State the oracle version next to every
  ratio and do not claim 100x where the corrected denominator says otherwise.
- **Deferral:** the exact ratio needs a quiet-machine Python run; the false claim cannot
  remain as written in the meantime.

### M1 (MEDIUM): the fast-path-off switch is compiled out outside `cfg(test)`, so the mandated real-case cross-check cannot run

- **Where:** `crates/xstate-core/src/engine/algorithm.rs:126` (the only call site),
  `:188-202` (`fast_path_enabled()` is `true` for `cfg(not(test))`; the `FAST_PATH`
  thread-local exists only under `cfg(test)`).
- **What:** `xstate-conformance` is a separate crate and links `xstate-core` without
  `cfg(test)`, so every conformance/golden run always takes the fast path. The handover
  §6.0 asks for "the whole conformance suite and every golden with the fast path
  disabled" — that is impossible without changing the crate.
- **Repro:** `git grep -n FAST_PATH -- crates` returns only `algorithm.rs` and
  `engine/fast_path_tests.rs`; `cargo test -p xstate-conformance --all-features` cannot
  observe the switch.
- **Mitigation already present:** the unit-test differential compares both paths on
  random machines and passes at `PROPTEST_CASES=100000` (334 s) — see "Verified sound".
  It does not exercise the 53 conformance charts on the general path.
- **Fix:** expose the switch as a non-default feature (`fast-path`), or read an
  environment variable once behind `cfg(debug_assertions)`, and add a conformance test
  that runs both modes; or move an equivalent of the 53-case runner into `xstate-core`
  unit tests. Do not leave the mandate item unachievable.
- **Deferral:** none needed for correctness of this commit; it is a verification gap.

### M2 (MEDIUM): the largest hot-path rewrites landed without adversarial/equivalence tests

**Status: MITIGATED in `5a3e1c2`.** The 17-machine fast-path adversarial matrix and the 34
adversarial direct-compiler documents found by the QA audit now live in the crate's test suite
and run in CI. The remaining process point (the rewrites shipped before those tests existed) is
history and cannot be changed without rewriting the commits.

- **Where and what:**
  - `32ffe52` (17 files, +717/−331): allocation pass and the semantic change of the
    default `actionErrorPolicy` to `fail`; **0** `#[test]` additions.
  - `5efc80e` (27 files, +1,989/−745): replaces the engine planning/selection and
    interpreter stepping, drops `FixedBitSet`; adds only two unit tests (`bits.rs`
    ranges, `machine/source.rs` JSON round trip). No differential test for the new
    plans/select path.
  - `6c4e652` (15 files, +439/−155): receipt `ActionList`, lazy timer cancellation,
    registry scan; 2 tests plus a 16-line runtime-alignment addition.
  - `9ec75e0` only later adds the 244-line fast-path differential and 259-line direct
    compiler equivalence, which retroactively exercises much of the new planning code.
- **Why it matters:** the handover §6.0 asks for tests with behaviour and specifically
  for an adversarial input the generator never produces. A bisect to `5efc80e` (before
  `9ec75e0`) had only the pre-existing suite and the conformance cases; a bug in the
  rewrite would have been found only if an existing case happened to cover it.
- **Repro:** `git show <hash> -- '*.rs' | Select-String '^\+.*#\[test\]'` (counts above);
  `git show 5efc80e --stat`.
- **Fix:** add the differential/equivalence tests in the same commit as the next such
  rewrite; keep the 100k-case run as a periodic gate. Not a defect found in current
  behaviour — the final 100k run and the conformance suite are green.

### M3 (MEDIUM): SECURITY.md and CODE_OF_CONDUCT.md point at reporting channels and documents that do not exist

**Status: FIXED in `7fe47c2`.** `SECURITY.md` now names GitHub private vulnerability reporting
(there is no author email in the manifest) and links a real threat-model page,
`docs/book/src/operations/security.md`, which documents the parser, snapshot, envelope,
inspector and axum controls with their code locations.

- **Where:** `SECURITY.md:11-13` says to email "via the contact address listed in the
  Cargo manifest author metadata"; `Cargo.toml:26` is
  `authors = ["Basil T T"]` with **no email**. GitHub private vulnerability reporting is
  offered only "once it is public", and the repository does not exist yet.
  `SECURITY.md:42-43` says the threat model "ships with the documentation site
  (`docs/book/operations/security.md`)"; `docs/book` does not exist in the tree.
  `CODE_OF_CONDUCT.md` enforcement repeats the nonexistent Cargo contact.
- **Repro:** `Select-String -Path Cargo.toml -Pattern authors`; `Test-Path docs\book`
  → `False`; the files were added by `cf541e6`.
- **Fix:** add a real contact (a dedicated security email, or enable GitHub private
  reporting once the remote exists), and either write the threat model at the referenced
  path or remove the forward reference. Code of Conduct enforcement needs the same
  working contact.
- **Deferral:** the GitHub remote is owner-gated; the false internal path is not.

### L1 (LOW): `2850cf8` is an empty commit whose message claims a 15-file renormalization

- **Where:** commit `2850cf8` "style: renormalize 15 working-tree files to LF".
- **What:** the commit's tree equals its parent's
  (`git rev-parse 2850cf8^{tree}` == `2850cf8^^{tree}`), `git diff-tree -r` is empty and
  `git show --numstat` prints nothing. No file was renormalized by this commit. The
  current checkout is nevertheless all-LF (`git ls-files --eol | select-string w/crlf`
  count 0), so the end state is fine; a report that cites this hash as the CRLF fix would
  be recording a fix that is not in history.
- **Fix:** drop or reword the commit before publishing (nothing is pushed), and correct
  the §6.1 finding text to state where the working-tree normalization actually came from
  (or that no tree change was needed because the index was already LF).

### L2 (LOW): several phase commits bundle multiple independent features, contrary to §8's one-logical-change rule

- **Where:** `24b407e` (79 files, +19,242/−98) bundles PostgreSQL stores, NATS and
  RabbitMQ brokers, the live SSE inspector and agents; `d277a71` (78 files, +12,267/−8)
  bundles typed macros, axum and Redis; `18e89c1` (64 files, +14,058/−10) bundles EDA and
  patterns; `0c81518` (48 files, +6,559/−51) bundles observability, inspector and CLI.
- **What:** each message enumerates the parts, so the description is accurate, but the
  commits are not individually reviewable or bisectable as one logical change. This is a
  process debt in the pre-handover history, not a defect in the tree.
- **Fix:** split future changes; no history rewrite is required for the owner's release
  unless they want one.

## Commit inventory

`files` is the commit's changed-file count, `+/−` its insertion/deletion count from
`git show --shortstat`, `tests` the number of added `#[test]` functions (plus new
data-driven conformance cases where noted). "match" means the subject/body describes the
changed files; "tested" means the commit itself adds tests or oracle-backed cases.

| Commit | Subject (short) | files | +/− | tests | Verdict |
|---|---|---:|---:|---:|---|
| `a3f0f84` | phase 0 skeleton, ADRs, conformance oracles | 68 | +7656 | 4 | match; both oracles wired |
| `3308aea` | engine, compiler, sync interpreter | 36 | +6855/−62 | 9 | match; 10 cases added |
| `db8874d` | snapshots layout v4, structure hash | 16 | +1334/−116 | 5 | match |
| `c6f92b8` | engine benchmarks, drop action clones | 4 | +130/−29 | 0 | match; perf change without new test |
| `59a007e` | bench lint allow | 1 | +1/−1 | 0 | match |
| `8f6fd13` | bench clippy | 1 | +1/−1 | 0 | match |
| `32ffe52` | allocation pass | 17 | +717/−331 | 0 | **finding M2** |
| `04a0e04` | register modules | 4 | +7 | 0 | match |
| `07eaa52` | tokio, builder, exporters, pure API | 95 | +5516/−54 | 44 | match |
| `bb4b339` | register graph module | 2 | +3 | 0 | match |
| `7b81661` | graph utilities, README quick start | 8 | +1425/−5 | 19 | match |
| `c1e0c87` | store contract/errors | 16 | +599/−25 | 1 | match |
| `74df9d5` | stores, locks, durable timers, idempotency | 32 | +10993/−13 | 213 | match; largest test batch |
| `60e53f2` | inspect/observability scaffolds | 7 | +484/−1 | 0 | match |
| `0c81518` | observability, inspector, CLI | 48 | +6559/−51 | 101 | match; 68 CLI tests counted exactly |
| `f257ea4` | EDA/patterns scaffolds, redact move | 10 | +117/−6 | 0 | match |
| `18e89c1` | EDA and patterns | 64 | +14058/−10 | 115 | match; **finding L2** |
| `d66a150` | macros/axum scaffolds | 6 | +538 | 0 | match |
| `d277a71` | macros, axum, Redis | 78 | +12267/−8 | 91 | match; **finding L2** |
| `813db24` | agents scaffold | 4 | +46 | 0 | match |
| `24b407e` | Postgres, NATS/RabbitMQ, live inspector, agents | 79 | +19242/−98 | 191 | match; **finding L2** |
| `a2a8fee` | py/facade scaffolds | 6 | +138 | 0 | match |
| `eac9a52` | SQS, bindings, facade, 0.1.0 prep | 70 | +7783/−227 | 46 | match; 29-pytest claim not re-collected |
| `d247434` | pre-release security/quality/parity fixes | 179 | +5872/−1172 | 75 | match; spot-checked claims |
| `116c65e` | QA handover | 1 | +583 | 0 | docs only |
| `b3b3e34` | implicit invoke-input divergence | 2 | +5 | 0 | docs only |
| `07cd16a` | note Python-side fix | 2 | +6/−5 | 0 | docs only |
| `eed5e35` | exit actions on final start; convergence | 15 | +188/−195 | 1 | match; 8 case expectations changed |
| `1abec8d` | eight oracle cases + fixture format | 38 | +2027/−46 | 0 | data-driven: +8 cases, runner support |
| `cfb0817` | eventless order/settling/final regions | 24 | +2309/−92 | 3 | match; +5 cases |
| `6caa8ab` | Python 0.12.0 / XState 5.33.3 alignment | 64 | +4257/−937 | 63 | match; later 0.12.1 drift findings |
| `90e3815` | deny pin-utils ignore | 1 | +1 | 0 | match |
| `5efc80e` | plans/select/bitset dispatch rewrite | 27 | +1989/−745 | 2 | **finding M2** |
| `6c4e652` | receipts/timers/registry pass | 15 | +439/−155 | 2 | **finding M2** |
| `9ec75e0` | fast path, direct compile | 19 | +1825/−367 | 7 + proptest | match; strongest tests, stale perf claim |
| `d4b2dea` | handover update | 1 | +129/−28 | 0 | docs; 0.12.0 state recorded |
| `e194300` | FileStore lock-stamp skip | 2 | +116/−7 | 2 | match; verified against Python |
| `4625e5a` | handover FileStore note | 1 | +18/−2 | 0 | docs only; "unreleased" is now stale |
| `cf541e6` | SECURITY/COC/CONTRIBUTING | 3 | +146 | 0 | **finding M3** |
| `2850cf8` | renormalize 15 files to LF | 0 | 0 | 0 | **finding L1 (empty)** |

## Verified sound

- **Commit descriptions match their diffs.** All 40 subjects/bodies enumerate the files
  they touch; the phase commits' broad scope is disclosed in the body, not hidden. No
  unrelated source or CI change was found outside the enumerated scope.
- **CI was not weakened.** No added `continue-on-error`, `if: false`, skipped gate or
  `#[ignore]`; `git grep` for `unsafe {`, `todo!(`, `unimplemented!(`, `dbg!(` and
  `#[ignore]` over `*.rs` returns nothing at HEAD.
- **Golden provenance is real.** `generate_expected.py` invokes the Node XState oracle
  (`oracle.mjs`) and the Python library; `inspect_golden.py`, `eda_golden.py`,
  `patterns_golden.py`, `agents_golden.py` and `export_golden.py` import
  `xstate_statemachine` and derive their fixtures from it. The conformance runner
  compares Rust against `expected.xstate`, not against a Rust-generated golden
  (`crates/xstate-conformance/tests/engine.rs:37-56`).
- **Current oracle parity.** Regenerating all 53 cases against Python 0.12.1 and XState
  5.33.3 produced 0 behavioural differences (the only diff is the recorded Python
  version); `export_golden.py --check` reports 53 `.mmd` and 53 `.puml` files clean.
  `conformance/divergences.generated.md` says "No divergences" and no case carries a
  stale `divergence` key.
- **Suites run for this review.** `cargo test -p xstate-core --lib --all-features`
  → 41 passed; `cargo test -p xstate-conformance --all-features --locked` → 1 + 4 + 5
  passed (all 53 cases in the engine test); the fast-path differential with
  `PROPTEST_CASES=100000` → passed in 334.45 s.
- **Direct compiler equivalence.** `machine/direct_tests` passes on every repository
  machine document, the 79 valid/invalid edge documents and 400 random machines, and
  compares the full fingerprint (states, transitions, warnings, source JSON, structure
  hash and declared events) with the parser path, including identical error strings.
- **`e194300` matches the Python implementation.** Python 0.12.1
  `persistence/file_store.py` `_stamp_is_fresh` uses the same own-pid,
  `0 <= age < stale_lock_after/2`, future/unparseable-rejected rule; the Rust helper
  has unit tests for own/fresh, older, other pid, future and unparseable stamps.
- **Line endings are currently correct.** `git ls-files --eol` has no `w/crlf` entry;
  the empty commit is a history defect, not a live format defect.
- **`0c81518`'s 68-CLI-test claim is exact:** 46 in `tests/cli.rs` + 12 in
  `tests/snapshots.rs` + 10 unit tests across `src/commands/*` and `ui.rs`.

## Limits / not covered by this section

- Behavioural review of the business logic in the large phase commits (persistence,
  brokers, axum, agents, bindings) belongs to the per-crate battle tests (§6.2); this
  section verifies history, tests and provenance.
- The 100k differential is random-machine testing, not a proof; the missing
  conformance-on-general-path run is finding M1.
- The Python 0.12.1 `send()` fast path changes a benchmark denominator, not semantics;
  the semantic conclusion relies on the 53-case regeneration above.

## Handover to the report owner

1. Decide the oracle pin first: regenerate all 53 fixtures with the same Python version
   that CI installs (0.12.0 or 0.12.1) and pin that exact version in
   `XSM_PYTHON_REQ`; this unblocks the conformance-fixtures job without touching the
   Rust engine.
2. Re-baseline the Python performance column against the corrected/released table and
   rewrite the 100x claims in README/budgets/handover before the release note repeats
   them.
3. Add a non-test fast-path switch (or a core-level conformance runner) so §6.0's
   general-path run is executable.
4. Add a real security contact and either write or remove the threat-model reference;
   drop or reword the empty `2850cf8` before the first push.
