# §6.1 Builds, gates and repository hygiene

Severity tally so far: CRITICAL 0 · HIGH 0 · MEDIUM 3 · LOW 3 (all open items tracked below).

> **Status update (head `903077c`, 2026-11-07).** The three MEDIUMs below are fixed:
> the Python oracle is pinned to 0.12.1 (`32881c3`), the CRLF worktree was renormalised
> (`2850cf8`), and SECURITY.md / CODE_OF_CONDUCT.md / CONTRIBUTING.md were added (`cf541e6`).
> Gates re-run on the current head: `cargo fmt --all --check` PASS; workspace clippy
> (`RUSTFLAGS=-D warnings`, `--workspace --all-targets --all-features`) PASS; workspace rustdoc
> (`RUSTDOCFLAGS=-D warnings`, `--workspace --no-deps --all-features`) PASS after the EDA fix
> `83b0b6e`; `cargo deny check` PASS (same cosmetic MPL-2.0 warning). The full workspace test
> suite was re-run on the final tree:
> `XSM_REQUIRE_SERVICES=1 cargo test --workspace --all-features --locked -j 4 --no-fail-fast`
> → **1156 passed, 0 failed** across 91 test binaries (unit, integration, cross-language and
> doctests; log in `docs/qa/work/workspace_test_2026-11-07.log`). MSRV:
> `cargo +1.88 check --workspace --locked` passes (log `docs/qa/work/msrv_check.log`). The
> Linux/WSL leg, facade feature matrix, package dry-run and llvm-cov coverage remain to be
> completed.

## Gates run on the main checkout (head `4625e5a`)

| Gate | Command | Result | Notes |
|---|---|---|---|
| Baseline tests | `cargo test --workspace --all-features --no-fail-fast -j 4` | PASS | full suite + doctests green; no failures anywhere in the run |
| rustfmt | `cargo fmt --all -- --check` | PASS | exit 0 |
| cargo-deny | `cargo deny check` | PASS | advisories ok, bans ok, licenses ok, sources ok; one cosmetic warning (unused `MPL-2.0` allow, `deny.toml:24`) |
| rustdoc `-D warnings` | not yet re-run | — | ran clean per handover; re-run in the clippy/doc pass |
| clippy pedantic `-D warnings` | not yet re-run | — | next pass |

Environment used: rustc/cargo 1.99.1 (pinned by `rust-toolchain.toml`), Windows 11, Node 24.18,
Python venv at `C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine\.venv\Scripts\python.exe`
with `xstate_statemachine` 0.12.0 importable.

## Windows fresh clone (full build from scratch)

`git clone` into `C:\Users\basil\Desktop\Projects\RustProjects\xstate_fresh_win` at head
`4625e5a`, then `cargo test --workspace --all-targets --all-features -j 4`.

- Build finished in 10m 58s from an empty `target/`.
- Every test target passed (xstate-agents 46+12+7+6, xstate-axum 25+18+6+6, conformance 1+4+5,
  xstate-core 41 unit + integration suites, xstate-eda, xstate-inspect, xstate-macros incl.
  trybuild, xstate-observability, xstate-patterns, xstate-persistence 39+71+22+1,
  xstate-statemachine facade 13+9, xsm CLI 16+49+11+17, xstate-tokio 6+5, doctests) — 0 failed.
- No backend skips were expected here: Docker service-dependent tests print their skip notes
  (they were satisfied with the `XSM_REQUIRE_SERVICES` unset default).
- Fresh-clone verdict: PASS on Windows. Linux leg in progress (WSL Ubuntu 2, rustc 1.99.1).

## Repository hygiene findings

### MEDIUM: CI installs the Python reference from `git main`; PyPI has 0.12.0
- `.github/workflows/ci.yml` (`XSM_PYTHON_REQ`) and its comment: the comment claims "0.12.0 is
  not on PyPI yet (PyPI has 0.11.0)" — stale. `pip index versions xstate-statemachine` shows
  0.12.0 on PyPI (released 2026-11-05).
- What: the integration and conformance-fixtures jobs install an unpinned `main` of the Python
  library, so CI could silently test against unreleased behaviour.
- Repro: read `ci.yml`; run `pip index versions xstate-statemachine`.
- Fix: pin to `xstate-statemachine[redis,sqlalchemy,nats,rabbitmq,sqs]==0.12.0` (integration)
  and `xstate-statemachine==0.12.0` (conformance-fixtures); verify the PyPI wheel ships the
  extras before switching. Owner-gated until the remote exists, but the file can be fixed now.

### MEDIUM: 15 working-tree files are CRLF while the index is LF
- What: `.gitattributes` declares `text=auto eol=lf`, the index is all-LF, but 15 files
  (incl. `Cargo.toml`, `LICENSE`, agent golden fixtures, `asyncapi_schema.json`) were checked
  out before the attributes took effect and sit as CRLF in the working tree.
- Repro: `git ls-files --eol | findstr crlf`.
- Fix: `git add --renormalize .` and commit the normalized worktree.

### MEDIUM: SECURITY.md, CODE_OF_CONDUCT.md, CONTRIBUTING.md are missing
- What: §6.1 and the release checklist require them; the handover flags them as missing.
- Fix: author them (policy text referencing the existing gates and the private-reporting route).

### LOW: `deny.toml` allows `MPL-2.0` but no crate in the tree uses it
- What: `cargo deny check` reports `license-not-encountered` for the `MPL-2.0` entry.
- Fix: remove the allowance or keep it with a reason; decide when tidying deny.toml.

### LOW: `xstate-py` fresh-clone build worked, but the binding tests were not run from the clone
- What: `cargo test` covers the Rust lib target (`test = false`), so `pytest` is the binding
  suite; it needs `maturin develop --release`. Scheduled in the xstate-py battle-test pass.

### LOW: no git tags exist; CHANGELOG `[0.1.0]` dated 2026-11-02 predates the tag
- What: changelog is historical/aspirational; tagging `v0.1.0` is the owner's call per §8.
- Fix: none needed in-repo; note for the release checklist.

## Verified sound

- Main checkout and a pristine Windows clone both pass the full all-features, all-targets suite
  (all crates, doctests included), with zero failures.
- rustfmt and cargo-deny are clean as claimed by the handover.
- `.gitattributes` enforces LF; the repository index is already all-LF.
- `publish-order.txt` covers exactly the publishable crates; `xstate-conformance` and
  `xstate-py` are `publish = false`.
