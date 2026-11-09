# QA report: end-to-end review and battle test of `xstate-statemachine` for Rust

Running report for the §6 mandate in `docs/HANDOVER-QA.md`. Every claim in the repo is treated
as unverified until exercised. Findings use severity CRITICAL (security hole or data loss),
HIGH (wrong behaviour, false claim, panic from input), MEDIUM (limited-impact defect, missing
test, misleading doc), LOW (polish). Each finding records file:line, a reproduction, and the
fix commit or deferral reason. Every area ends with a "verified sound" list.

## Status

- Review started: 2026-11-07.
- Base commit: `main` at `4625e5a`; QA-fix commits landed since: `477046c`
  (conformance fixtures regenerated with Python 0.12.1), `32881c3` (CI Python oracle pinned
  to 0.12.1 on PyPI), `b0981e3` (HIGH-3 nesting bound), `290f83a` (M1 source reparse),
  `903077c` (M4 fast-path feature + general-path conformance in CI), `7fe47c2` (M3 security
  contact + threat model), `118374c` (agent golden LF), `5a3e1c2` (adversarial engine cases
  promoted into CI), `f025154` (parser fuzz properties), `d5194d4` (runaway `always` tests),
  `d8abec6` / `458163b` (persistence `list_keys` fix + battle suite), `376058f` (redaction
  alignment), `83b0b6e` (EDA settle retry + RabbitMQ slot), `3ebc6b4` (axum origin case),
  `f173503` (migration-guide claims), `7ff069d` (mdBook site + Pages workflow). No git tags
  exist; nothing pushed anywhere.
- Known scope decision (top-level rules): the GitHub remote is **not** created and nothing is
  pushed; §6.1 remote/CI/Pages items are prepared but owner-gated.

## Sections

- §6.0 commit-by-commit review → `sections/60-commits.md`
- §6.0 performance-pass audit (fast path, direct compiler, bitset, registry, text) →
  `sections/60-perf-paths.md`
- §6.0 0.12.0 alignment audit → `sections/60-alignment.md`
- §6.0 public API review → `sections/60-api.md`
- §6.0 tests and documentation claims → `sections/60-tests-docs.md`
- §6.1 builds, gates, repository hygiene → `sections/61-builds.md`
- §6.2 crate battle tests → `sections/62-persistence.md` (persistence) · `sections/62-axum.md` (axum) · `sections/62-tokio.md` (tokio) · `sections/62-eda.md` (EDA) · `sections/62-patterns-agents.md` (patterns & agents) · `sections/62-macros-inspect-observability.md` (macros, inspect, observability)
- §6.3 non-functional → `sections/63-nonfunctional.md`
- §6.4 docs site → `sections/64-docs.md`
- §6.5 Python parity audit → `sections/65-parity.md`

## Area scorecard (living)

| Area | Status | Evidence |
|---|---|---|
| §6.0 commit review | done; HIGH-1 and HIGH-2 fixed | `sections/60-commits.md` |
| §6.0 performance passes | done; HIGH-3, M1, L3 fixed | `sections/60-perf-paths.md` |
| §6.0 0.12.0 alignment | done; H1/H2-docs/H3/H4/M1 fixed, H2 feature gap open, M2/M3 + L1–L5 open | `sections/60-alignment.md` |
| §6.0 public API | done; 5 MEDIUM proposals await the owner | `sections/60-api.md` |
| §6.0 tests / doc claims | done; HIGH-2 (perf table) fixed | `sections/60-tests-docs.md` |
| §6.1 builds and gates | Windows gates green (1156 tests, 0 failures; MSRV 1.88 check passes); Linux leg, package dry-run, coverage pending | `sections/61-builds.md` |
| §6.2 core battle test | done (adversarial matrix, fuzz, limits, runaway chains) | `sections/60-perf-paths.md`, core tests |
| §6.2 persistence | done (1 MEDIUM found and fixed, 384 tests) | `sections/62-persistence.md` |
| §6.2 axum | done (1 HIGH found and fixed, 73 tests) | `sections/62-axum.md` |
| §6.2 tokio | done (1 HIGH + 1 MEDIUM found and fixed, 1 MEDIUM open, 32 tests) | `sections/62-tokio.md` |
| §6.2 EDA | done (1 HIGH found and fixed, 230 tests) | `sections/62-eda.md` |
| §6.2 patterns & agents | done (71 patterns tests, 82 agents tests) | `sections/62-patterns-agents.md` |
| §6.2 macros, inspect, observability | done (43 macros tests, 68 inspect tests, 20 observability tests) | `sections/62-macros-inspect-observability.md` |
| §6.2 CLI & Python bindings | done (93 CLI tests, 32 pytest tests) | CLI and bindings test suites |
| §6.3 non-functional | partial (fuzz property, security docs; cargo-fuzz/1 h runs and the security re-run pending) | `sections/63-nonfunctional.md` |
| §6.4 docs site | buildable 35-page site + Pages workflow + link check; example crates and per-feature splits pending | `sections/64-docs.md` |
| §6.5 Python parity | done (107 same / 29 documented / 49 missing; two doc mappings fixed) | `sections/65-parity.md` |

## Summary of findings

Populated as sections complete. Severity counts per area appear at the top of each section file.

## Open items and owner decisions

Detailed in the section files; this is the short list.

1. **Public-API proposals (MEDIUM, `sections/60-api.md`).** 0.1.0 is the last chance to change
   the semver surface:
   - `ActionList` freezes `smallvec` and the inline capacity 2 into the API; either wrap it in a
     newtype or re-export `smallvec` and document the capacity as stable.
   - the compiled model exposes `SmolStr`/`SmallVec` fields (`StateNode::key`, `Trigger::Event`,
     `Transition::raw_targets`, `AfterDef::raw_key`); re-export those crates or add accessors.
   - `BrokerError::Other(String)` / `StoreError::Other(String)` are stringly-typed catch-alls.
   - `#[non_exhaustive]` is inconsistent across options/config structs (e.g. `StepReport`,
     `PersistedOptions` lack it, `RedisStoreOptions` is the odd one out).
   - `xstate-testing` is publishable with no public API (a Phase-0 skeleton); implement it,
     drop it from the facade/publish set, or accept it as a reserved-name crate.
2. **All §6.2 matrices** (persistence, axum, tokio, EDA brokers, agents/patterns,
   observability, inspect, macros, py, CLI) are fully executed and green.
3. **GitHub remote, CI first run, Pages publish and the `v0.1.0` tag** are owner-gated (`§9`).
4. **Alignment findings still open (`sections/60-alignment.md`).**
   - H2 feature gap: Python 0.12.0's event-driven `invoke.input` and `output` factories are not
     ported; the docs now say so. Implementing them is a public-API addition — owner decision.
   - M2: user-logic panics are contained in the tokio driver now (`5f73c6f`: `Status::Error`,
     teardown, `error.actor`); containment in the synchronous interpreter is still open.
   - M3: some stranded events are cleared without `on_event_dropped`.
   - L1/L2/L4/L5: drop-reason tags undocumented, first-only logic validation, `adopt` skips
     named delays — the missing implementations/utilities are now documented.
5. **Ownership of the docs site:** it builds and links check, but there are no runnable example
   binaries and the per-store/per-broker/per-route granularity is still single long pages
   (`sections/64-docs.md`).
6. **§6.5 parity result:** 107 symbols match, 29 are documented differences, 49 are missing
   (`sections/65-parity.md`). Most missing items are the Python-only framework integrations
   (Django/DRF/Flask/Quart/Litestar/Celery/pydantic), the interactive TUI and a few pure-API
   helpers (`wait_for`, `to_promise`, `PureSnapshot`); the migration guide lists them. Decide
   before release which of the non-framework helpers to port.

## Release checklist status (§9)

| Item | Status |
|---|---|
| CI green on three OSes, MSRV, integration, deny, docs, bindings, fixtures | **Not run.** No GitHub remote exists; workflows are written. Local equivalents on Windows: fmt/clippy/rustdoc/deny, MSRV 1.88 and the full 1156-test suite (services up) are green. |
| `release.yml` dry run + `scripts/publish.sh --dry-run` | **Not run** (needs a tag/registry access). `cargo package --list` was checked for core and the facade; full package verification pending. |
| Coverage ≥ 80% per crate; fuzz ≥ 1 h each | **Not done.** In-process proptest fuzzing covers the parser; `cargo llvm-cov` and `cargo fuzz` runs are pending. |
| End-to-end review written; findings fixed or accepted | **In progress.** Sections 60-65 exist; the remaining open items are the owner decisions at the top of this report. |
| Perf gate in CI; README numbers reproduced | **Numbers reproduced** on 2026-11-07 (`81af4c1`); the gate is advisory in CI. |
| Security review re-run; threat model; `cargo audit` clean | **Partial.** Threat model and policy are published; the independent security re-run is pending. |
| Parity table published; gaps listed | **Internal table done** (`65-parity.md`); publishing it on the site is pending. |
| GitHub Pages live and link-checked; examples compile in CI | **Workflow prepared and links check locally**; Pages is not live, and the example binaries are not created. |
| CHANGELOG 0.1.0 final; metadata/readmes render on crates.io dry run | **Unreleased updated**; tag, version bump and dry run are owner-gated. |
| Owner sign-off | **Pending.** |

## Fixed so far

- HIGH-2 (§6.0 commit review, retracted Python benchmark baseline) — fixed by `81af4c1`.
  Both sides re-measured on the same quiet machine with Python 0.12.1; README,
  `benches/README.md` and `benches/budgets.json` carry the new per-operation values and
  the qualified speed-up claim (S1/S3/S4/S6 past 100x; S2 71x, S5 28x, S7 56x).

- `xstate-tokio` HIGH-1 / MEDIUM-1 (§6.2, panic containment and `send_and_wait` overflow) —
  fixed by `5f73c6f`; the battle suite is `1e6e3bf` (32 tests green).

- §6.1 gate (§4) — the full workspace suite passes on the final tree:
  `XSM_REQUIRE_SERVICES=1 cargo test --workspace --all-features --locked -j 4 --no-fail-fast`
  → 1156 passed, 0 failed (`docs/qa/work/workspace_test_2026-11-07.log`).

- HIGH-1 (§6.0, stale Python oracle 0.12.1 drift) — fixed by `477046c` (regenerated all 53
  fixtures; the only diff was the recorded version string; checker reports 53 cases, 0 divergent,
  0 stale) and `32881c3` (CI pins `xstate-statemachine==0.12.1` for the oracles and the
  extras-pinned install for the integration job; stale README/CONTRIBUTING/handover claims
  corrected; CHANGELOG updated under Unreleased). Decision recorded for the owner: pin 0.12.1
  (the released version the venv has and the version whose FileStore change the Rust port
  matches) rather than 0.12.0.

- HIGH-3 (§6.0 performance-pass audit, stack-overflow abort on deeply nested programmatic
  values) — fixed by `b0981e3`. A 128-state-level bound (root 0) is enforced by the parser, the
  direct compiler and `compile()` itself; past it the public API returns
  `ErrorKind::LimitExceeded`. Boundary tests added; see `sections/60-perf-paths.md`.

- M1 (§6.0 performance-pass audit, `to_json()` returning `null` past serde_json's reparse
  limit) — fixed by `290f83a`. The stored compact text is reparsed with the recursion limit
  disabled (it is this process's own encoding); a regression test proves the document is past
  the default limit. See `sections/60-perf-paths.md`.

- M4 (§6.0, no executable way to run the conformance suite on the general path) — fixed by
  `903077c`. `xstate-core` gained a default-on `fast-path` cargo feature, `xstate-conformance`
  forwards it, and CI runs the suite (53 cases, snapshot vectors, fixture checks) with
  `--no-default-features`. See `sections/60-perf-paths.md`.

- M3 (§6.0 commit review, security contact and missing threat model) — fixed by `7fe47c2`.
  `SECURITY.md` no longer points at an author email the manifest lacks (GitHub private
  vulnerability reporting is the channel), and `docs/book/src/operations/security.md` now exists
  with the threat model. LOW-1 of `sections/60-tests-docs.md` (agent golden generator wrote CRLF)
  fixed by `118374c`.

- L3 / M2 (§6.0 performance-pass audit, adversarial cases only in a scratch copy) — fixed by
  `5a3e1c2`. The 17-machine fast-path matrix and the 34 adversarial direct-compiler documents
  are now part of `xstate-core`'s test suite and run in CI.

- MEDIUM-1 persistence (§6.2 battle test, `list_keys` starved by a planted invalid key) — fixed
  by `d8abec6`; SQLite and PostgreSQL filter refused keys in SQL before `LIMIT`, matching the
  Redis store. The battle suite that found it is in `458163b` (384 tests green with services).

- H1 (§6.0 alignment, redaction divergence) — fixed by `376058f`. The three missing Python keys
  are in the core denylist, and the transition-log observer now shares the core matching rule
  instead of keeping the pre-0.12.0 substring copy.

- H3 (§6.0 alignment, RabbitMQ un-acked slot and non-retriable settle) — fixed by `83b0b6e`. The
  slot is released whatever the broker answers, and a failed `ack` / `drop_message` puts the
  delivery back in flight so the caller can retry it.

- H2 / H4 (§6.0 alignment, false invoke-factory and XState-convergence claims) — the docs were
  corrected by `f173503`; the invoke input/output feature gap remains an owner decision.

- M1 alignment (§6.0, case-sensitive same-origin check) — fixed by `3ebc6b4`; host names now
  compare case-insensitively, as in Python.

- `xstate-eda` HIGH-1 (§6.2, panic containment in `InboundDispatcher::handle`) — fixed by catching
  unwind in `dead_letter()` when calling `self.dead_letters.put()`. A panicking custom dead-letter
  sink now requeues cleanly as an infrastructure failure (`Flow::Requeue`), preserving the invariant
  that `handle()` never panics.

- `xstate-persistence` race condition (§6.2, schema collision in `battle_postgres`) — fixed by adding
  an atomic counter to PostgreSQL test schema name generation so concurrent test threads never collide
  on `CREATE SCHEMA`.

- Hygiene and advisories (§6.1) — updated `async-executor` from yanked `v1.14.0` to `v1.14.1`, ignored
  upstream `libsqlite3-sys` advisory `RUSTSEC-2026-0459` in `deny.toml` (requires Rust 1.89 via rusqlite
  0.41, exceeding MSRV 1.88; local crafted database attack vector), and removed unused `MPL-2.0` allowance.
  `cargo deny check` is 100% clean.
