# GEMINI.md — Instructions for Gemini & AI Coding Agents

This repository is `xstate-statemachine-rs`: an enterprise-grade, high-performance visual statechart runtime for Rust, fully compatible with the **[XState v5](https://stately.ai/docs/xstate)** specification and interoperable with Python `xstate-statemachine` 0.12.1.

See [`AGENTS.md`](AGENTS.md) for the complete operational manual, invariants, quality gates, and publishing protocols.

---

## Quick Reference Commands

Run these quality gates locally before committing:

```bash
# 1. Format verification
cargo fmt --all --check

# 2. Strict Clippy linting (warnings treated as errors)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 3. Security, licenses, and banned crates audit
cargo deny check

# 4. Documentation build with warnings treated as errors
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# 5. Core test suite across all 15 crates
cargo test --workspace --all-features -j 4 --no-fail-fast

# 6. Documentation book build and relative link check
mdbook build docs/book
python scripts/linkcheck.py docs/book/book

# 7. Conformance fixture freshness (checks against Node XState & Python 0.12.1)
python conformance/tools/generate_expected.py --check
python conformance/tools/export_golden.py --check

# 8. Performance gate check & Native FSM benchmark (< 5 ns budget)
python scripts/perf_gate.py --no-run
cargo bench -p xstate-core --bench core -- native_fsm
```

---

## Workspace Architecture (15 Crates)

- **`xstate-core`**: Core sans-I/O statechart engine, 69.8 ns fast-path bitsets (`send_fast`, `send_str`, `step_ref`), zero-cost native enum state machines (< 5 ns `Fsm` & `FsmInstance`), AST code generator (`Machine::to_rust_code`), bidirectional XState v5 JSON exporter (`ExportXState`), snapshots, Mermaid/PlantUML exporters.
- **`xstate-tokio`**: Actor runtime, bounded mailboxes, backpressure policies, precision timers.
- **`xstate-persistence`**: Storage engines (PostgreSQL, SQLite, Redis, File), optimistic locking, durable deadlines.
- **`xstate-eda`**: CloudEvents 1.0, transactional outbox, dead letters, AsyncAPI 3.0, NATS, RabbitMQ, SQS, Redis Streams.
- **`xstate-patterns`**: Distributed sagas with compensation, circuit breaker, exponential backoff with jitter.
- **`xstate-axum`**: Web API, REST endpoints, Server-Sent Events (SSE), WebSockets, RFC 9457 Problem Details.
- **`xstate-inspect`**: Stately visual inspector protocol, live SSE server, deterministic JSONL recording & replay.
- **`xstate-observability`**: Prometheus metric set via `metrics` facade.
- **`xstate-agents`**: LLM agent loops as statecharts: tool allow-lists, token budget guards, human approvals.
- **`xstate-macros`**: `statechart!` DSL, `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `xstate_from_json!`, `xstate_inline_json!`.
- **`xstate-testing`**: Virtual simulated clocks, path walking, coverage.
- **`xstate-statemachine-cli`**: `xsm` CLI binary (validate, inspect --live, diagram, docs, simulate, `codegen` with `--check` drift gate).
- **`xstate-statemachine`**: Top-level facade crate re-exporting all components behind feature flags.
- **`xstate-conformance`**: Conformance suite runner (`publish = false`).
- **`xstate-py`**: PyO3 Python bindings (`publish = false`).

---

## Critical Invariants

1. **Safety:** `#![forbid(unsafe_code)]` enforced across all production crates.
2. **Zero-Allocation Fast Path:** Steady-state microsteps must allocate 0 heap bytes (`FixedBitSet`, `SmolStr`, `SmallVec`). Native enum transitions via `Fsm` execute in 1.15–1.76 ns p50 with 0 heap bytes.
3. **Panic Containment:** Actors and message dispatchers must catch panics (`catch_unwind`) and transition to error states without crashing.
4. **Wire Parity with Python:** Layout `v4` snapshots and 16-hex character structure hashes must match the Python library down to the byte.
5. **Code Generation & Drift Verification:** `xsm codegen --check` acts as a CI gate verifying committed Rust code matches source statecharts. Macro file imports must use `include_str!` tracking.
6. **Read-Only Python Repo:** Never modify files in the cousin Python repo (`../PythonProjects/xstate-statemachine`).
7. **No Premature Release:** Do not push to git remotes or crates.io without explicit instruction from the owner.
