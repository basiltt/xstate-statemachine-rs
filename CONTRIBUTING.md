# Contributing

Thanks for your interest in `xstate-statemachine` for Rust. The library is a Rust port of the
Python `xstate-statemachine` library with an XState v5 compatible engine; see `README.md`,
`docs/guide/` and `docs/adr/` for context.

## Development environment

- Stable Rust as pinned by `rust-toolchain.toml` (components rustfmt, clippy,
  llvm-tools-preview). The workspace MSRV is 1.88; the `postgres` feature needs 1.94 (sqlx 0.9).
- The conformance suite regenerates expectations from two oracles: the real XState v5 engine
  under Node (`conformance/tools/xstate_oracle`) and the Python library 0.12.0
  (`conformance/tools/generate_expected.py`). Golden fixtures are generated from the Python
  library; never hand-edit a golden.
- Docker services for the backend tests: see §4 of `docs/HANDOVER-QA.md` for the exact
  containers and environment variables.

## The gates

Every commit must keep these green:

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features --no-fail-fast   # add XSM_REQUIRE_SERVICES=1 with Docker up
cargo deny check
```

Additional gates for changes touching the Python-facing surface:

```bash
"<python venv>" -I conformance/tools/generate_expected.py --check
"<python venv>" -I conformance/tools/export_golden.py --check
```

## Commit and change conventions

- One logical change per commit; conventional commit messages (`feat:`, `fix:`, `docs:`,
  `test:`, `perf:`, `chore:`).
- Behaviour changes need tests. Never weaken a test to make it pass and never skip a golden;
  if a golden must change, regenerate it from Python with its generator script and explain why.
- The lint configuration in `Cargo.toml` (`[workspace.lints]`) is enforced in CI with
  `-D warnings`; do not add `#[allow]` without a justifying comment.
- Public API changes before the first release still go through the same review: update the
  changelog under `Unreleased` and the relevant rustdoc.

## Areas and owners

The crate map and the feature-flag matrix are in `README.md`. Design decisions live in
`docs/adr/`; the conformance cases and oracle tooling are documented in `conformance/README.md`.
For larger changes, open an issue first and agree the approach with the maintainer.
