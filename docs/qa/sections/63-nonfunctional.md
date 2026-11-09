# §6.3 Non-functional: performance, leaks, errors, security, standards

Partial pass. This section records what was exercised and what still needs a dedicated run.

## Performance

- The hot-path audit and mutation testing are in `sections/60-perf-paths.md`; the fast path,
  direct compiler, bitset, event index, registry, interned text and lazy timer cancellation were
  reviewed for correctness and their invariant tests promoted into CI (`5a3e1c2`).
- **HIGH-2 remains open:** the README/budgets Python baseline is the retracted 0.12.0 table and
  the "past 100x" wording needs a quiet-machine re-measure with the 0.12.1 venv
  (`sections/60-commits.md`). Do not release with the current numbers.
- No CI timing gate is enforced on this machine; `scripts/perf_gate.py` is advisory in CI.

## Leaks and resource use

- Persistence lock release on panic/drop, connection caps and SSE/WS slot release are covered by
  the persistence and axum battle suites.
- The tokio actor lifecycle (child stop, timer cancellation, mailbox bounds) is **not yet**
  battle-tested in this pass; it is on the §6.2 list.
- Long-running leak checks (RSS over many interps, `cargo llvm-cov` memory) were not run.

## Error handling

- The parser and snapshot entry points are fuzzed in-process: 2000 arbitrary JSON values and 2000
  arbitrary strings per run must produce a value or a typed error, never a panic (`f025154`).
- User-logic panics are not contained: a panic in an action/guard/service is not converted to
  `on_error`, can leak child actors and leaves async-service invokes hanging (alignment M2, open).
- Stranded-event reporting: some error/deferred queues are still cleared without
  `on_event_dropped` (alignment M3, open).

## Security

- Threat model and private-reporting policy: `SECURITY.md` + `docs/book/src/operations/security.md`
  (`7fe47c2`).
- Redaction now matches Python 0.12.0 (H1, `376058f`); same-origin host comparison is
  case-insensitive (M1, `3ebc6b4`).
- The security review has **not** been re-run on the final tree; the earlier independent reviews
  (commit `d247434`) found no CRITICAL issues, but §9 requires a re-run before release.
- `cargo audit` / `cargo deny` pass as part of the CI configuration; `cargo deny check` was run
  locally (one cosmetic unused `MPL-2.0` allowance).

## Standards and tooling

- `cargo fmt --all --check`, workspace clippy with `-D warnings` (all targets, all features) and
  workspace rustdoc with `-D warnings` pass on the current tree.
- MSRV: `cargo +1.88 check -p xstate-conformance --no-default-features` passes, proving the
  feature-gated dependency declaration is valid on 1.88.
- `cargo-fuzz` targets run for ≥ 1 h each are **not** done; the in-process proptest is the current
  substitute.
- `cargo-semver-checks`, `cargo-geiger`, `cargo-udeps` and `cargo-typos` advisories were not run
  in this pass; the public-API review (`sections/60-api.md`) covers the semver surface by
  inspection.
