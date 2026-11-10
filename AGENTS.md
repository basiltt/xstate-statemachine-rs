# AGENTS.md — Agent & Contributor Operational Manual for `xstate-statemachine-rs`

Operational brief and rulebook for AI coding agents (Claude, Codex, Antigravity, Cursor, Windsurf, Copilot) and human maintainers working on `xstate-statemachine-rs`.

---

## 1. Project Mission & Identity

`xstate-statemachine-rs` is a high-performance, production-grade statechart library for Rust, fully compatible with the **[XState v5](https://stately.ai/docs/xstate)** specification and the **[Stately Studio](https://stately.ai/editor)** visual editor.

### Key Tenets
1. **Extreme Performance:** Sub-microsecond transitions (**69.8 ns p50** on `send_fast` fast path; >14.3M ops/s; **73.8 ns** bare engine step) with **0 heap allocations** on the fast path.
2. **Byte-Level Interoperability:** 100% binary snapshot format compatibility (Layout `v4`) and identical structure hashes (`SHA256(topology)[:16]`) with the Python library [`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine) 0.12.1. Shared SQLite, PostgreSQL, and Redis stores.
3. **Turnkey Production Toolset:** 15 modular workspace crates providing actor-model concurrency on Tokio, durable persistence, transactional outbox messaging (NATS, RabbitMQ, SQS, Redis Streams), resilience patterns (sagas, circuit breakers), Axum web integration, Stately visual inspection, Prometheus metrics, and LLM agent guards.
4. **Absolute Safety:** `#![forbid(unsafe_code)]` enforced across all production crates. Complete panic containment in async dispatch and actors.

---

## 2. Workspace Architecture Map (15 Crates)

```
crates/
├── xstate-core              # Sans-I/O W3C SCXML / XState v5 algorithm, bitsets, direct compiler, pure API
├── xstate-tokio             # Tokio actor runtime, bounded mailboxes, overflow policies, timers
├── xstate-persistence       # Durable stores (SQLite, Postgres, Redis, File), locks, durable deadlines
├── xstate-eda               # CloudEvents 1.0, transactional outbox, DLQ, AsyncAPI 3.0, NATS, RabbitMQ, SQS
├── xstate-patterns          # Distributed Sagas, exponential backoff with jitter, circuit breakers
├── xstate-axum              # Web integration: REST, Server-Sent Events (SSE), WebSockets, RFC 9457
├── xstate-inspect           # Stately Inspector protocol, SSE live server, JSONL deterministic recording
├── xstate-observability     # Prometheus metric set via metrics facade, label hygiene, timings
├── xstate-agents            # LLM agents as statecharts: tool loops, token budget guards, approvals
├── xstate-macros            # Proc macros: statechart! DSL, #[derive(TypedEvent)], #[derive(TypedState)]
├── xstate-testing           # Test harness, virtual clocks, path walkers, mock logic
├── xstate-conformance       # 53 external conformance test runners (dev only, publish = false)
├── xstate-py                # PyO3 Python acceleration module xstate_rs (publish = false)
├── xstate-statemachine-cli  # `xsm` CLI binary (validate, inspect --live, diagram, docs, simulate)
└── xstate-statemachine      # Unified facade crate re-exporting all crates behind feature flags
```

---

## 3. Strict Quality Gates & Verification Protocol

Every change must pass all quality gates locally before committing. Never weaken a test, skip an assertion, or delete a golden to pass a gate.

### Standard Verification Suite
```bash
# 1. Code formatting
cargo fmt --all --check

# 2. Clippy linting across all targets and features (warnings as errors)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 3. Cargo license, dependency, and vulnerability security audit
cargo deny check

# 4. Documentation build and rustdoc lints
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# 5. Core test suite
cargo test --workspace --all-features -j 4 --no-fail-fast

# 6. Documentation book build and relative link verification
mdbook build docs/book
python scripts/linkcheck.py docs/book/book

# 7. Conformance fixture freshness (checks against Node XState & Python 0.12.1)
python conformance/tools/generate_expected.py --check
python conformance/tools/export_golden.py --check

# 8. Python PyO3 bindings tests & type checks (if Python venv active)
pytest crates/xstate-py/tests/test_bindings.py
mypy --strict crates/xstate-py/python/xstate_rs

# 9. Performance gate check (regression threshold: 1.25x baseline)
python scripts/perf_gate.py --no-run
```

---

## 4. Backend Service Testing with Docker

Integration tests for PostgreSQL, Redis, NATS, RabbitMQ, and AWS SQS run against real servers. By default, tests skip if a service is unreachable.

To enforce strict validation (skipping tests causes build failure):
```bash
# 1. Start all 5 backend containers
docker run -d --name xsm-redis -p 6379:6379 redis:7
docker run -d --name xsm-pg -p 5432:5432 -e POSTGRES_USER=xsm -e POSTGRES_PASSWORD=xsm -e POSTGRES_DB=xsm postgres:16
docker run -d --name xsm-nats -p 4222:4222 nats:2 -js
docker run -d --name xsm-rabbit -p 5672:5672 rabbitmq:3
docker run -d --name xsm-sqs -p 9324:9324 softwaremill/elasticmq-native

# 2. Run the full integration test suite
XSM_REQUIRE_SERVICES=1 cargo test --workspace --all-features -j 4
```

---

## 5. Architectural Invariants for Agents & Contributors

When implementing code or modifying crates, preserve these core invariants:

1. **`#![forbid(unsafe_code)]`:** No `unsafe` blocks are allowed in any library crate.
2. **Zero-Allocation Fast Path:** The plain microstep engine (`Plans::is_plain`) must never perform heap allocations in steady state. Active state sets use `FixedBitSet`; identifiers use stack-inlined `SmolStr`; actions use `SmallVec<[ActionDef; 2]>`.
3. **Panic Containment:**
   - User actions and guards executing in `xstate-tokio` actors must catch panics (`std::panic::catch_unwind`), transition the actor to `Status::Error`, and emit `error.actor`.
   - In `xstate-eda`, custom dead-letter sink panics must be caught and converted to `Flow::Requeue` so `InboundDispatcher::handle` never panics.
4. **State Nesting Depth Limit:** State nesting is capped at 128 levels (`MAX_STATE_NESTING = 128`) to prevent stack overflow aborts on recursive parsing.
5. **Snapshot Wire Parity:**
   - Snapshot layout version is `v4`.
   - Structure hash is 16 lowercase hex characters (`SHA256(canonical_topology)[:16]`).
   - Keys in JSON snapshots must preserve exact order (`preserve_order` feature in `serde_json`).
6. **Python Repository Isolation:** The cousin repository at `../PythonProjects/xstate-statemachine` is strictly **READ-ONLY**. Never write to, modify, or delete files in the Python repo.
7. **Eager Plans Precomputation & Direct Selection:** Transition plans (`Plans`) are computed eagerly during machine assembly (`Machine::from_data`) so `machine.plans()` is a plain pointer dereference without `OnceLock` synchronization. Non-parallel statecharts bypass `Enabled` SmallVec collection via direct `select_in_chain`.
8. **Steady-State Fast Path (`send_fast`, `send_str`, `step_ref`):** When no internal queue, timers, observers, or deferred events are pending, the interpreter invokes the step engine without `Receipt` allocation or `Arc<Machine>` cloning.

---

## 6. Git & Commit Protocol

1. **Atomic Commits:** One logical change per commit.
2. **Conventional Commit Syntax:** `feat(...)`, `fix(...)`, `test(...)`, `docs(...)`, `chore(...)`, `refactor(...)`.
3. **Attribution:** Include the co-authorship trailer:
   ```text
   Co-Authored-By: Codex <noreply@openai.com>
   ```
4. **Changelog:** Always record changes in [`CHANGELOG.md`](CHANGELOG.md) under `## [Unreleased]`.
5. **No Remote Creation / Pushing without Owner Authorization:** Do not push to git remotes or publish to crates.io / PyPI unless explicitly instructed by the repository owner.

---

## 7. Crates.io Publishing Order

Crates must be published in topological dependency order (defined in [`scripts/publish-order.txt`](scripts/publish-order.txt)):

```
1. xstate-core
2. xstate-macros
3. xstate-tokio
4. xstate-persistence
5. xstate-testing
6. xstate-inspect
7. xstate-observability
8. xstate-agents
9. xstate-eda
10. xstate-patterns
11. xstate-axum
12. xstate-statemachine (facade)
13. xstate-statemachine-cli (`xsm` binary)
```

Run a packaging dry-run with:
```bash
cargo package --workspace --no-verify
bash scripts/publish.sh --dry-run
```
