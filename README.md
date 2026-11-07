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
| Engine | Yes | XState v5 JSON (hierarchy, parallel, history, final, `after`, `always`, `invoke`, actors, `systemId`, every built-in action, composite guards), snapshots layout v4, structure hash, pure API, builder, JSON / Mermaid / PlantUML export, graph utilities. `invoke.input` is a static JSON value: the Python 0.12.0 input/output factories are not ported. Follows XState v5 where Python diverges (ADR-0001) |
| Async actors | Yes | `xstate-tokio`: bounded mailbox with overflow policies, `after` on `tokio::time`, async services, child machines, `sendTo` / `sendParent` |
| Persistence stores | Yes | Memory, file, SQLite, Redis, PostgreSQL (the SQLAlchemy layout; no MySQL, Django or other ORM stores) |
| Locks, timers, inbox, log | Yes | Optimistic and pessimistic locking, `persisted`, durable deadlines and scanner, idempotency inbox, transition log, snapshot migrations, `adopt` |
| EDA: envelope, dispatcher, outbox, DLQ, AsyncAPI | Yes | Byte-identical envelopes, per-subject ordering, retries, replay, AsyncAPI 3.0. The outbox is at-least-once (relay with claim leases) and has memory and SQLite stores only, no PostgreSQL outbox. The async `run_once` handles a batch on scoped threads and acks it afterwards ([differences](docs/guide/migrating-from-python.md)) |
| Brokers | Redis Streams, NATS, RabbitMQ, SQS: Yes. Kafka: not yet | Same topics, bodies and attributes as the Python adapters |
| Patterns | Yes | Retry with jitter, circuit breaker (applied with `wrap`, no decorator), saga builder, choreography router (blocking brokers only); identical chart JSON |
| Observability | Partial | Prometheus metric set through the `metrics` facade; OpenTelemetry, structlog, Sentry not yet |
| Inspector | Yes | JSONL recording and replay, SSE server (`xsm inspect --live`), axum WebSocket endpoint; `@statelyai/inspect` 0.7.2 |
| CLI commands | Partial (9 of the Python commands) | `validate`, `inspect`, `diagram`, `docs`, `simulate`, `paths`, `snapshots`, `replay`, `info`; not yet `coverage`, `asyncapi`, `dlq`, `generate-template`, `list-templates`, `new`, `plugins`, `update`, `setup`. `snapshots` reads SQLite and file stores (no `memory://`) and `--stale` compares version labels like Python (`--stale-by hash` compares structure) |
| Typed DSL | Rust-native | `statechart!`, `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `MachineBuilder`; the Python class DSL (metaclass scanning, naming-convention hooks) is not copied |
| HTTP | Yes (axum) | Registry, REST and per-event routes, SSE, WebSocket, RFC 9457 problem details, idempotency keys; Flask, Django, DRF, Litestar, Channels, Celery integrations are Python-framework specific and not ported |
| Agents | Yes (core) | Tool-loop, supervisor, pipeline and debate charts, budgets, allow-lists, approvals, structured output, trace; provider adapters and framework bridges are out of scope |
| Python bindings | In progress | `xstate-py` (PyO3): the Python API backed by the Rust engine |
| Testing helpers | Partial | Graph walking, coverage targets and `stub_logic` in `xstate-core`; the pytest plugin has no Rust counterpart (`cargo test`) |

## Performance

The Python library's own cross-library benchmark (scenarios S1 to S7 of
`benchmarks/competitors/` in xstate-statemachine 0.12.0, sync engine) is mirrored one to one in
[`crates/xstate-core/benches/python_parity.rs`](crates/xstate-core/benches/python_parity.rs): the
same machines, the same logic (JSON context; S2's guard and action are Rust closures, the
equivalent of Python's callables) and the same work per operation as the Python `hot()` loop.
Median (p50) per operation on one developer machine (Intel Core i7-11850H, 31.7 GiB, Windows 11
Pro, rustc 1.99.1, system allocator; CPython 3.14.6 on the same host). The machine was not
quiet, so each performance pass is measured interleaved with the previous pass's tree in one
session (best of seven runs each for the scenarios, three for the engine benchmarks). The third
pass's session ran about 20 % slower than the second pass's recording, so compare within a
column pair; the speed-up column divides the Python time by this pass's number as measured:

| Scenario | One operation | Second pass | This pass | Python 0.12.0 | Speed-up | 100x target |
|---|---|---|---|---|---|---|
| S1 `flat_toggle` | one `send` on A ⇄ B | 186 ns | 116 ns | 12.35 µs | **106x** | 124 ns |
| S2 `guarded_context` | one guarded `send` whose action increments `count` | 214 ns | 176 ns | 10.54 µs | 60x | 105 ns |
| S3 `hierarchical` | `CROSS` + `DEEP` across a two-level hierarchy (two sends) | 367 ns | 252 ns | 28.37 µs | **113x** | 284 ns |
| S4 `parallel` | `TOGGLE1` + `TOGGLE2` on two regions (two sends) | 456 ns | 302 ns | 37.86 µs | **125x** | 379 ns |
| S5 `construction` | `Machine::from_value` of S3's machine + `new` + `start` | 8.28 µs | 4.76 µs | 79.0 µs | 17x | 790 ns |
| S6 `many_instances` | per instance of 1 000: `new` + `start` + one `send` | 406 ns | 329 ns | 21.9 µs | 67x | 219 ns |
| S7 `delayed_transition` | new `SimulatedClock`, build, `new` + `start`, advance 1 ms, see `done` | 5.58 µs | 3.89 µs | 118 µs | 30x | 1.18 µs |

`benches/budgets.json` keeps the lower of the recorded baselines per benchmark. S1, S3 and S4
are past 100x; S2 and S6 are within 1.7x and 1.5x of it; construction (S5, S7) is not, and
cannot be while every operation builds a machine. What bounds each:

- **Dispatch (S1, S3, S4)**: a send whose single enabled transition is *plain* (nothing it exits
  or enters has actions, timers, invocations or final semantics, and the machine has no history
  states) skips the general microstep: it clears the exit range's configuration bits, runs the
  transition's actions and sets the precomputed entry set. What is left per send is transition
  selection (the event-key lookup, then a walk up from the active leaf or down the active
  regions), the report, and the `Receipt`: an `Arc` clone and drop of the machine (about 11 ns
  of S1's 116 ns) plus the active leaves. A receipt without the machine handle would be an API
  change (`state_ids()` / `matches()` would need the machine); not done.
- **S2** is S1's dispatch plus the guard and the action: two registry lookups by name, two
  closure calls that each look `count` up in the JSON context, and an `Arc<str>` clone of the
  action name into the receipt's `ActionList` (another atomic pair). Reaching 105 ns needs the
  logic resolved once per interpreter (an index per action and guard instead of a name lookup)
  and receipts that name actions without reference counting; both are visible in the API.
- **Construction (S5, S7)**: plain documents compile straight from the JSON value (20
  allocations for S3's machine instead of 58; anything else still goes through
  `MachineConfig`). The rest is the compiled arena itself: a full-id `Arc<str>` per state, the
  child list of every compound state and the target list of every transition (public `Vec`
  fields of `StateNode` / `Transition`), the state and transition tables, the id index, the
  source document as compact JSON text (for `to_json`) and the `Arc`. On the Windows system
  allocator an allocation and its free cost 50 to 100 ns, and a `StateNode` is about 700 bytes
  to build and move. S7 adds the `after` entry's strings and the interpreter's timer and effect
  buffers; without the build it is about 0.85 µs. 100x (0.79 / 1.18 µs) is out of reach for an
  operation that builds a machine.
- **S6** is `new` + `start` + one send + dropping two receipts and the interpreter, with no
  allocation; the initial entry set is now cached per machine and the shared clock borrowed.
  What remains is constructing, moving and dropping an ~830-byte interpreter, the start
  macrostep and the two receipts' `Arc` pairs.

The engine benchmarks that predate the parity scenarios, second pass against this pass
(interleaved in the same session, best of three each):

| Benchmark | Second pass | This pass | What it measures |
|---|---|---|---|
| `send_flat` | 222 ns | 120 ns | one event on a two-state machine, no actions |
| `step_flat` | 135 ns | 82 ns | the bare `engine::step`, no interpreter |
| `send_guard_assign` | 338 ns | 186 ns | guard plus a JSON `assign` |
| `send_nested_3` | 254 ns | 130 ns | three-level hierarchy, cross-branch transition |
| `send_parallel_2` | 288 ns | 169 ns | two orthogonal regions toggling |
| `build_50_states` | 191 µs | 90 µs | JSON text to `Machine`, 50 states |
| `snapshot_get` | 2.12 µs | 2.05 µs | serialise a 50-state machine's snapshot |
| `snapshot_restore` | 12.9 µs | 9.8 µs | parse, validate and restore |

`snapshot_get` and `snapshot_restore` run code this pass barely touched: their differences are
within the session's run-to-run spread, which the untouched `build_phase_*` benchmarks put at
about 25 %.

Reproduce both sides:

```bash
# Rust (this repository): run, then gate against benches/budgets.json and print the speed-ups
cargo bench -p xstate-core --bench python_parity --bench core -- --noplot
python scripts/perf_gate.py --no-run

# Python (the xstate-statemachine repository, its virtualenv active)
python benchmarks/competitors/run.py --lib xstate_sync
```

Treat single runs with care: on a desktop with a browser and other applications running, medians
move by 1.5x to 2x between runs, which is why the table reports the best of several runs and why
the CI gate is advisory. [`benches/README.md`](benches/README.md) describes the benchmarks and
the gate: `scripts/perf_gate.py` compares every median with `baseline_p50_ns x allowed_slowdown`
(1.25) from [`benches/budgets.json`](benches/budgets.json) and fails on a regression;
`--relative` first divides out the machine's speed, and CI runs it with `--advisory --relative`.

## Interoperability with the Python library

Compatibility is a design goal (ADR-0003). The **stores and brokers** are tested in both
directions against the real Python library (`XSM_PYTHON`, see [Development](#development)): Rust
writes and Python reads, and the other way round. Everything else is pinned by **goldens generated
from the Python library** (`conformance/tools/*.py`) and compared with Rust's output, i.e. one
direction, Python to Rust: snapshots, envelopes, AsyncAPI documents, saga and circuit-breaker
charts, Mermaid / PlantUML diagrams and inspector recordings. Compatible at the byte level:

- **Snapshots**: layout v4 (field order, `state_ids`, `configuration`, `deadlines`, ...) and the
  **machine structure hash** (SHA-256 prefix of the canonical node-shape JSON), so a snapshot saved
  by Python restores in Rust and vice versa. The hash is byte-identical; the snapshot text equals
  Python's `get_snapshot()` once whitespace is removed (Rust writes compact JSON, Python indents by
  2), which a conformance test asserts on every fixture where the engines agree.
- **Store layouts**: SQLite (`statecharts`, `deadlines`, inbox, log, outbox and dead-letter
  tables), file records (`.xsm.json`), Redis (`snap:{key}` hashes, Lua scripts, inbox, log) and
  PostgreSQL (`xsm_*` tables of the SQLAlchemy schema, with schema-version refusal).
- **Envelopes**: CloudEvents 1.0 JSON, sortable ids, and the broker wire formats (Redis Streams
  fields, NATS JetStream subjects and `Nats-Msg-Id`, RabbitMQ queues, SQS queue names, bodies and
  `ce_type` attribute).
- **Inspector recordings**: the Rust observer's `.jsonl` output is identical line by line to
  Python `InspectorPlugin` recordings of the same run (golden comparison); the protocol is pinned
  to `@statelyai/inspect` 0.7.2.
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
[`conformance/divergences.generated.md`](conformance/divergences.generated.md) — empty since the
Python engine converged on XState v5 (its `xstate-v5-engine-parity` branch, after 0.12.0); the
eight cases that used to differ are now ordinary conformance cases on both sides.

## Development

Quality gates (all of them run in CI; the toolchain is stable, MSRV 1.88; the `postgres` feature,
and so `full`, needs Rust 1.94 because of `sqlx` 0.9, which is why the MSRV job checks everything
except `postgres`):

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
one they print `skipping ...` and pass, so a green local run does not mean the backends ran.
Set `XSM_REQUIRE_SERVICES=1` to make a missing service a panic instead of a skip; CI's
`integration` job sets it, starts all five servers and also fails if any test line starts with
`skipping`.

| Variable | Default | Server |
|---|---|---|
| `XSM_REDIS_URL` | `redis://127.0.0.1:6379/` | `docker run -d -p 6379:6379 redis:7` |
| `XSM_POSTGRES_URL` | `postgres://xsm:xsm@127.0.0.1:5432/xsm` | `docker run -d -p 5432:5432 -e POSTGRES_USER=xsm -e POSTGRES_PASSWORD=xsm -e POSTGRES_DB=xsm postgres:16` |
| `XSM_NATS_URL` | `nats://127.0.0.1:4222` | `docker run -d -p 4222:4222 nats:2 -js` (JetStream enabled) |
| `XSM_AMQP_URL` | `amqp://guest:guest@127.0.0.1:5672/%2f` | `docker run -d -p 5672:5672 rabbitmq:3` |
| `XSM_SQS_ENDPOINT` | `http://127.0.0.1:9324` | `docker run -d -p 9324:9324 softwaremill/elasticmq-native` |

Tests use a unique key, topic, queue or table per test and clean up after themselves.
`XSM_PYTHON` points at a Python with the Python library installed for the cross-language
tests, which skip (panic under `XSM_REQUIRE_SERVICES=1`) when it cannot import
`xstate_statemachine`. The default assumes this layout, with the two repositories as cousins:

```text
<projects>/RustProjects/xstate-statemachine-rs/          # this repository
<projects>/PythonProjects/xstate-statemachine/.venv/     # the Python library with its virtualenv
```

Then:

```bash
cargo test --workspace --all-features --locked
```

### Conformance and releases

Regenerate conformance expectations (needs Node 20 or newer and the Python library) and the
Mermaid / PlantUML goldens (Python library only); both have a `--check` mode that CI uses:

```bash
npm ci --prefix conformance/tools/xstate_oracle
python conformance/tools/generate_expected.py
python conformance/tools/export_golden.py
```

CI installs the Python library from PyPI, pinned to the released version the conformance fixtures
were generated from (0.12.1), so the fixture-freshness checks cannot silently drift onto
unreleased Python behaviour.

A release is a `vX.Y.Z` tag: `.github/workflows/release.yml` runs the gates, the whole CI workflow
(so the integration job and the fixture-freshness checks must be green) and
`scripts/publish.sh --dry-run`, then (after approval) publishes the crates in the order of
`scripts/publish-order.txt`.

## License

MIT, see [LICENSE](LICENSE).
