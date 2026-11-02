# xstate-statemachine (Rust)

[![crates.io](https://img.shields.io/crates/v/xstate-statemachine.svg?style=flat-square)](https://crates.io/crates/xstate-statemachine)
[![docs.rs](https://img.shields.io/docsrs/xstate-statemachine?style=flat-square)](https://docs.rs/xstate-statemachine)
[![CI](https://img.shields.io/github/actions/workflow/status/basiltt/xstate-statemachine-rs/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/basiltt/xstate-statemachine-rs/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/rustc-1.88+-orange.svg?style=flat-square)](https://github.com/basiltt/xstate-statemachine-rs/blob/main/Cargo.toml)
[![License](https://img.shields.io/crates/l/xstate-statemachine.svg?style=flat-square)](LICENSE)
[![Python library](https://img.shields.io/pypi/v/xstate-statemachine?style=flat-square&label=python%20library)](https://pypi.org/project/xstate-statemachine/)

**Statecharts for Rust, compatible with [XState v5](https://stately.ai/docs/xstate) and the
[Stately editor](https://stately.ai/editor).** Design the chart visually, export it as JSON,
supply the named guards, actions and services in Rust, and run it: hierarchy, parallel regions,
history, delayed transitions, invoked services, actors, snapshots and durable persistence.

This is the Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine). Same JSON dialect, same
snapshot format and store layouts (a Rust worker can resume an instance a Python worker saved),
same wire formats, same CLI; built for speed, with Rust's type system doing the checking.

**Find your way:** [Why](#why) · [Quick start](#quick-start) · [Workspace](#workspace) ·
[Feature parity](#feature-parity-with-the-python-library) · [Performance](#performance) ·
[Interoperability](#interoperability-with-the-python-library) · [Guide](#guide) ·
[Development](#development)

## Why

A statechart lists the states that exist and the only legal moves between them, so impossible
combinations of flags cannot be constructed, and the same diagram is both the documentation and
the program. XState and the Stately editor are the most widely used tooling for that, but they
live in JavaScript. This crate runs the *same chart JSON* in Rust with XState v5 semantics
(verified against the real XState engine on every conformance case), and it shares its snapshots,
databases, brokers and recordings with the Python library, so a polyglot fleet can migrate one
service at a time instead of rewriting a state model. Compared with the other Rust FSM crates it
is a *runtime* you can load from JSON and persist, not a compile-time-only macro, and it is 30 to
70 times faster per event than the Python engine.

> **Status: 0.1.0.** Engine, async runtime, persistence (SQLite, Redis, PostgreSQL),
> observability, inspector, CLI, event-driven architecture with Redis Streams, NATS, RabbitMQ and
> SQS, patterns, typed DSL, axum integration and agents have landed (phases 0 to 7). Remaining:
> Kafka, the rest of the CLI commands, and the Python bindings (`xstate-py`, in progress).
> See the [changelog](CHANGELOG.md), [`docs/feasibility-report.md`](docs/feasibility-report.md)
> and the [ADRs](docs/adr/).

## Quick start

```toml
[dependencies]
xstate-statemachine = "0.1"   # add features for what you need: tokio, sqlite, eda, ...
```

```rust
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{
  "id": "door", "initial": "closed", "context": {"opens": 0},
  "states": {
    "closed": {"on": {"OPEN": {"target": "opened", "guard": "isUnlocked", "actions": "countOpen"}}},
    "opened": {"on": {"CLOSE": "closed"}}
  }
}"#)?;
let logic = MachineLogic::dynamic()
    .guard("isUnlocked", |ctx, _event, _params| Ok(ctx["locked"] != true))
    .action("countOpen", |scope| {
        let n = scope.context["opens"].as_u64().unwrap_or(0) + 1;
        scope.context["opens"] = n.into();
        Ok(())
    });
let mut door = SyncInterpreter::new(machine, logic);
door.start()?;
door.send("OPEN")?;
assert_eq!(door.state_ids(), ["door.opened"]);
# Ok::<(), xstate_statemachine::core::Error>(())
```

Next: [getting started](docs/guide/getting-started.md), or the
[Stately editor workflow](docs/guide/stately-editor.md).

## Workspace

| Crate | Purpose |
|---|---|
| [`xstate-statemachine`](crates/xstate-statemachine) | the facade: one dependency that re-exports every library crate behind feature flags mirroring the Python extras, plus a `prelude` |
| [`xstate-core`](crates/xstate-core) | machine model, JSON parser/validator, sans-I/O step engine, sync interpreter, snapshots, pure API, builder, exporters, graph utilities |
| [`xstate-tokio`](crates/xstate-tokio) | async interpreter on tokio: mailbox, timers, invoked services, actors |
| [`xstate-persistence`](crates/xstate-persistence) | stores (memory, file, SQLite, Redis, PostgreSQL), locks, `persisted` wrapper, durable deadlines, idempotency inbox, audit log, migrations |
| [`xstate-testing`](crates/xstate-testing) | test helpers (skeleton; stub logic, simulated clock and path walking live in `xstate-core` today) |
| [`xstate-conformance`](crates/xstate-conformance) | fixture model and runner for the conformance suite (dev only, not published) |
| [`xstate-inspect`](crates/xstate-inspect) | Stately Inspector protocol: observer, allow-lists and redaction, JSONL recording and replay, SSE server |
| [`xstate-observability`](crates/xstate-observability) | `metrics`-facade observer with the Python `PrometheusPlugin` metric set and label hygiene |
| [`xstate-eda`](crates/xstate-eda) | CloudEvents envelopes, broker contract and adapters (Redis Streams, NATS, RabbitMQ, SQS), inbound dispatcher, transactional outbox, dead letters, AsyncAPI 3.0 |
| [`xstate-patterns`](crates/xstate-patterns) | retry policy, circuit breaker, saga builder, choreography router |
| [`xstate-macros`](crates/xstate-macros) | `#[derive(TypedEvent)]`, `#[derive(TypedState)]` and the compile-time checked `statechart!` DSL |
| [`xstate-axum`](crates/xstate-axum) | registry, REST / per-event routes, SSE and WebSocket streaming, problem details, idempotency, Stately Inspector endpoint |
| [`xstate-agents`](crates/xstate-agents) | LLM agents as statecharts: tool-loop chart, budgets as guards, tool registry with allow-lists and approvals, runner, trace, multi-agent helpers |
| [`xstate-py`](crates/xstate-py) | Python bindings (PyO3) backed by the Rust engine; in progress, not published to crates.io |
| [`xstate-statemachine-cli`](crates/xsm) | the `xsm` command-line tool: validate, inspect (`--live`), diagram, docs, simulate, paths, snapshots, replay, info |

Feature flags (all opt-in, see [`docs/guide/features.md`](docs/guide/features.md)):
`xstate-persistence/{sqlite,redis,postgres}` and `xstate-eda/{sqlite,redis-streams,nats,rabbitmq,sqs}`,
surfaced by the facade as `tokio`, `persistence`, `sqlite`, `redis`, `postgres`, `eda`,
`redis-streams`, `nats`, `rabbitmq`, `sqs`, `patterns`, `observability`, `inspect`, `macros`,
`axum`, `agents`, `testing` and `full`.

## Feature parity with the Python library

Against `xstate-statemachine` 0.12.0. "Yes" means ported with the same behaviour at the wire and
storage level; differences in engine semantics are in
[`docs/guide/migrating-from-python.md`](docs/guide/migrating-from-python.md).

| Area | Rust | Notes |
|---|---|---|
| Engine | Yes | XState v5 JSON (hierarchy, parallel, history, final, `after`, `always`, `invoke`, actors, `systemId`, every built-in action, composite guards), snapshots layout v4, structure hash, pure API, builder, JSON / Mermaid / PlantUML export, graph utilities. Follows XState v5 where Python diverges (ADR-0001) |
| Async actors | Yes | `xstate-tokio`: bounded mailbox with overflow policies, `after` on `tokio::time`, async services, child machines, `sendTo` / `sendParent` |
| Persistence stores | Yes | Memory, file, SQLite, Redis, PostgreSQL (the SQLAlchemy layout; no MySQL, Django or other ORM stores) |
| Locks, timers, inbox, log | Yes | Optimistic and pessimistic locking, `persisted`, durable deadlines and scanner, idempotency inbox, transition log, snapshot migrations, `adopt` |
| EDA: envelope, dispatcher, outbox, DLQ, AsyncAPI | Yes | Byte-identical envelopes, per-subject ordering, retries, replay, relay with claim leases, AsyncAPI 3.0 |
| Brokers | Redis Streams, NATS, RabbitMQ, SQS: Yes. Kafka: not yet | Same topics, bodies and attributes as the Python adapters |
| Patterns | Yes | Retry with jitter, circuit breaker, saga builder, choreography router; identical chart JSON |
| Observability | Partial | Prometheus metric set through the `metrics` facade; OpenTelemetry, structlog, Sentry not yet |
| Inspector | Yes | JSONL recording and replay, SSE server (`xsm inspect --live`), axum WebSocket endpoint; `@statelyai/inspect` 0.7.2 |
| CLI commands | Partial (9 of the Python commands) | `validate`, `inspect`, `diagram`, `docs`, `simulate`, `paths`, `snapshots`, `replay`, `info`; not yet `coverage`, `asyncapi`, `dlq`, `generate-template`, `new`, `plugins`, `update`, `setup` |
| Typed DSL | Rust-native | `statechart!`, `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `MachineBuilder`; the Python class DSL (metaclass scanning, naming-convention hooks) is not copied |
| HTTP | Yes (axum) | Registry, REST and per-event routes, SSE, WebSocket, RFC 9457 problem details, idempotency keys; Flask, Django, DRF, Litestar, Channels, Celery integrations are Python-framework specific and not ported |
| Agents | Yes (core) | Tool-loop, supervisor, pipeline and debate charts, budgets, allow-lists, approvals, structured output, trace; provider adapters and framework bridges are out of scope |
| Python bindings | In progress | `xstate-py` (PyO3): the Python API backed by the Rust engine |
| Testing helpers | Partial | Graph walking, coverage targets and `stub_logic` in `xstate-core`; the pytest plugin has no Rust counterpart (`cargo test`) |

## Performance

Median (p50) per operation on one developer machine, from [`benches/budgets.json`](benches/budgets.json)
(recorded 2026-11-02, after the allocation pass; criterion benches in
`crates/xstate-core/benches`). The Python figures are the Python library's own benchmark
medians (`benchmarks/budgets.json`).

| Benchmark | Rust p50 | Python p50 | Speed-up | What it measures |
|---|---|---|---|---|
| `send_flat` | 0.42 µs | 14 µs | 33x | one event on a two-state machine, no actions |
| `send_guard_assign` | 0.52 µs | 16 µs | 31x | guard plus `assign` on a dynamic context |
| `send_nested_3` | 0.65 µs | 35 µs | 54x | three-level hierarchy, cross-branch transition |
| `send_parallel_2` | 0.57 µs | 39 µs | 69x | two orthogonal regions toggling |
| `snapshot_get` | 1.3 µs | 12 µs | 9x | serialise a 50-state machine's snapshot |
| `snapshot_restore` | 9 µs | 23 µs | 2.6x | parse, validate and restore, including logic validation |
| `step_flat` | 0.33 µs | n/a | | the bare `engine::step`, no interpreter |
| `build_50_states` | 180 µs | 115 µs\* | | JSON text to `Machine`, 50 states (parse 44 µs, config 49 µs, compile 62 µs) |

\* Python's figure builds a much smaller machine from an already parsed dict, so it is not
comparable; machine construction is a once-per-process cost. `persisted_memory` (load, send, save
on the memory store; Python 87 µs, target 8 µs) has no baseline yet.

[`benches/README.md`](benches/README.md) lists the targets. The budgets file also defines the
relative performance gate (a machine speed factor is estimated from the median ratio across all
rows, then every row must stay within `allowed_slowdown` = 1.25 of its baseline); CI currently
builds every benchmark (`cargo bench --no-run`) but does not run the gate.

## Interoperability with the Python library

Compatibility is a design goal (ADR-0003) and is tested in both directions against the real
Python library (`XSM_PYTHON`, see [Development](#development)): Rust writes and Python reads, and
the other way round. Byte-compatible:

- **Snapshots**: layout v4 (field order, `state_ids`, `configuration`, `deadlines`, ...) and the
  **machine structure hash** (SHA-256 prefix of the canonical node-shape JSON), so a snapshot saved
  by Python restores in Rust and vice versa.
- **Store layouts**: SQLite (`statecharts`, `deadlines`, inbox, log, outbox and dead-letter
  tables), file records (`.xsm.json`), Redis (`snap:{key}` hashes, Lua scripts, inbox, log) and
  PostgreSQL (`xsm_*` tables of the SQLAlchemy schema, with schema-version refusal).
- **Envelopes**: CloudEvents 1.0 JSON, sortable ids, and the broker wire formats (Redis Streams
  fields, NATS JetStream subjects and `Nats-Msg-Id`, RabbitMQ queues, SQS queue names, bodies and
  `ce_type` attribute).
- **Inspector recordings**: `.jsonl` sessions identical line by line to the Python
  `InspectorPlugin`; the protocol is pinned to `@statelyai/inspect` 0.7.2.
- **Saga and circuit-breaker charts**: the builders emit the same machine JSON.
- **AsyncAPI 3.0 documents** and the **Mermaid / PlantUML diagrams** of the fixture machines.
- **HTTP behaviour**: problem details and the `Idempotency-Key` semantics.

Not the same: transition semantics where the Python engine deviates from XState v5 (event
ordering in a few cases, `reenter`, wildcards and engine events, multi-target transitions); see
the migration guide. A mixed fleet shares state, so some internal events may be ordered
differently until both libraries converge.

## Guide

- [Getting started](docs/guide/getting-started.md): install, run a chart, async, typed DSL, CLI
- [Stately editor to Rust](docs/guide/stately-editor.md): export the JSON, run it, what is supported
- [Persistence](docs/guide/persistence.md): stores, `persisted`, locks, durable timers, inbox, log
- [Event-driven architecture](docs/guide/eda.md): envelopes, dispatcher, outbox, dead letters, brokers
- [Migrating from Python](docs/guide/migrating-from-python.md): API mapping and semantic differences
- [Feature flags](docs/guide/features.md): every feature of every crate
- [ADRs](docs/adr/), [feasibility report](docs/feasibility-report.md), [changelog](CHANGELOG.md)

## Conformance

The engine is specified by executable fixtures in [`conformance/`](conformance/README.md). Each
case is run through the real XState v5 engine (Node) and through the Python library; the Rust
engine must match XState's observations and reproduce Python's snapshot bytes and structure hash.
Disagreements between the two oracles are listed in
[`conformance/divergences.generated.md`](conformance/divergences.generated.md).

## Development

Quality gates (all of them run in CI; the toolchain is stable, MSRV 1.88):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features        # RUSTFLAGS="-D warnings" in CI
cargo test --workspace --all-features
cargo test --workspace --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo deny check                                             # cargo install cargo-deny --locked
cargo bench --workspace --benches --no-run                   # the benchmarks must build
```

### Backend tests need servers

The tests of the Redis, PostgreSQL, NATS, RabbitMQ and SQS backends talk to real servers. Without
one they print `skipping ...: no <service> at <url>` and pass, so a green local run does not mean
the backends ran; CI's `integration` job starts all five and fails if a test skipped for a missing service.

| Variable | Default | Server |
|---|---|---|
| `XSM_REDIS_URL` | `redis://127.0.0.1:6379/` | `docker run -d -p 6379:6379 redis:7` |
| `XSM_POSTGRES_URL` | `postgres://xsm:xsm@127.0.0.1:5432/xsm` | `docker run -d -p 5432:5432 -e POSTGRES_USER=xsm -e POSTGRES_PASSWORD=xsm -e POSTGRES_DB=xsm postgres:16` |
| `XSM_NATS_URL` | `nats://127.0.0.1:4222` | `docker run -d -p 4222:4222 nats:2 -js` (JetStream enabled) |
| `XSM_AMQP_URL` | `amqp://guest:guest@127.0.0.1:5672/%2f` | `docker run -d -p 5672:5672 rabbitmq:3` |
| `XSM_SQS_ENDPOINT` | `http://127.0.0.1:9324` | `docker run -d -p 9324:9324 softwaremill/elasticmq-native` |

Tests use a unique key, topic, queue or table per test and clean up after themselves.
`XSM_PYTHON` points at a Python with the Python library installed (default: the `.venv` of a
sibling `PythonProjects/xstate-statemachine` checkout) for the cross-language tests, which skip
when it cannot import `xstate_statemachine`. Then:

```bash
cargo test --workspace --all-features --locked
```

### Conformance and releases

Regenerate conformance expectations (needs Node 20 or newer and the Python library):

```bash
npm ci --prefix conformance/tools/xstate_oracle
python conformance/tools/generate_expected.py
```

A release is a `vX.Y.Z` tag: `.github/workflows/release.yml` runs the gates and
`scripts/publish.sh --dry-run`, then (after approval) publishes the crates in the order of
`scripts/publish-order.txt`.

## License

MIT, see [LICENSE](LICENSE).
