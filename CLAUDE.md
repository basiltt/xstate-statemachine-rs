# CLAUDE.md — Instructions for Claude & AI Agents

This repository is `xstate-statemachine-rs`: an enterprise-grade, high-performance visual statechart runtime for Rust, compatible with XState v5 and interoperable with Python `xstate-statemachine` 0.12.1.

See [`AGENTS.md`](AGENTS.md) for the full operational manual, invariants, and architectural guidelines.

---

## Build, Test & Quality Gates

Run these commands to verify any changes:

```bash
# Format check
cargo fmt --all --check

# Strict linting (warnings are treated as errors)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Security, dependency bans, and licensing audit
cargo deny check

# Documentation build (rustdoc)
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Unit and integration tests (across all 15 crates)
cargo test --workspace --all-features -j 4 --no-fail-fast

# Documentation book build and linkcheck
mdbook build docs/book
python scripts/linkcheck.py docs/book/book

# Conformance suite check (checks against Node XState & Python 0.12.1)
python conformance/tools/generate_expected.py --check

# Performance regression gate (against budgets.json)
python scripts/perf_gate.py --no-run
```

---

## Workspace Architecture (15 Crates)

- **`xstate-core`**: Core sans-I/O statechart engine, 69.8 ns fast-path bitsets (`send_fast`, `send_str`, `step_ref`), XState v5 JSON parser, snapshots, Mermaid/PlantUML exporters.
- **`xstate-tokio`**: Actor runtime, bounded mailboxes, backpressure policies, precision timers.
- **`xstate-persistence`**: Storage engines (PostgreSQL, SQLite, Redis, File), optimistic locking, durable deadlines.
- **`xstate-eda`**: CloudEvents 1.0, transactional outbox, dead letters, AsyncAPI 3.0, NATS, RabbitMQ, SQS, Redis Streams.
- **`xstate-patterns`**: Distributed sagas with compensation, circuit breaker, exponential backoff with jitter.
- **`xstate-axum`**: Web API, REST endpoints, Server-Sent Events (SSE), WebSockets, RFC 9457 Problem Details.
- **`xstate-inspect`**: Stately visual inspector protocol, live SSE server, deterministic JSONL recording & replay.
- **`xstate-observability`**: Prometheus metric set via `metrics` facade.
- **`xstate-agents`**: LLM agent loops as statecharts: tool allow-lists, token budget guards, human approvals.
- **`xstate-macros`**: `statechart!` DSL, `#[derive(TypedEvent)]`, `#[derive(TypedState)]`.
- **`xstate-testing`**: Virtual simulated clocks, path walking, coverage.
- **`xstate-statemachine-cli`**: `xsm` CLI binary.
- **`xstate-statemachine`**: Top-level facade crate re-exporting all components behind feature flags.
- **`xstate-conformance`**: Conformance suite runner (`publish = false`).
- **`xstate-py`**: PyO3 Python bindings (`publish = false`).

---

## Critical Invariants

1. **Safety:** `#![forbid(unsafe_code)]` in all library crates.
2. **Zero-Allocation Fast Path:** Steady-state microsteps must allocate 0 heap bytes (`FixedBitSet`, `SmolStr`, `SmallVec`).
3. **Panic Containment:** Actors and message dispatchers must catch panics (`catch_unwind`) and transition to error states without crashing.
4. **Wire Parity with Python:** Layout `v4` snapshots and 16-hex character structure hashes must match the Python library down to the byte.
5. **Read-Only Python Repo:** Never modify files in the cousin Python repo (`../PythonProjects/xstate-statemachine`).
6. **No Premature Release:** Do not push to git remotes or crates.io without explicit instruction from the owner.
