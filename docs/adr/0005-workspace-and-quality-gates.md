# ADR-0005: Workspace, toolchain and quality gates

- **Status:** accepted (Phase 0, 2026-11-01)

## Decision

| Topic | Choice |
|---|---|
| Edition / MSRV | Rust 2024 edition; MSRV **1.88** (bumped only in a minor release, checked by `cargo msrv` in CI) |
| Workspace | `crates/xstate-core`, `crates/xstate-tokio`, `crates/xstate-persistence`, `crates/xstate-testing`, `crates/xstate-conformance` (dev-only), `crates/xsm` (CLI). Later phases add `xstate-macros`, `xstate-eda`, `xstate-patterns`, `xstate-observability`, `xstate-inspect`, `xstate-axum`, `xstate-agents`, `xstate-py` |
| Unsafe | `#![forbid(unsafe_code)]` in every crate |
| Lints | workspace `[workspace.lints]`: `missing_docs`, `unreachable_pub`, clippy `pedantic` + `unwrap_used`/`expect_used` in library crates (`-D warnings` in CI) |
| Errors | `thiserror` enums in libraries; `anyhow`/`miette` only in the `xsm` binary |
| Logging | `tracing` only; no `println!`/`log` in library crates |
| Serialization | `serde` + `serde_json`; SIMD parsers behind an optional feature later |
| Supply chain | `cargo deny` (advisories, licenses MIT/Apache-2.0/BSD/ISC/Unicode/Zlib, bans on duplicate major versions), `cargo audit` |
| Tests | unit + doctests, `proptest`, `insta`, conformance fixtures; coverage via `cargo llvm-cov` with a 90% gate once Phase 1 lands |
| Benchmarks | `criterion` + `budgets.json` perf gate (relative, like Python's `perf_gate.py`) |
| Docs | `#![deny(rustdoc::broken_intra_doc_links)]`, README included as crate docs, mdbook guide later |
| Release | `cargo release`, `cargo dist` for `xsm` binaries, `cargo semver-checks` before publishing |
| CI | GitHub Actions: fmt, clippy, test (ubuntu/windows/macos), msrv, deny, audit, docs, conformance oracles (Node + Python) |
