# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Every crate of the
workspace shares one version number. The minimum supported Rust version (MSRV) is 1.88 and is
raised only in a minor release.

## [Unreleased]

### Fixed

- `xstate-core`: `start()` landing directly in a top-level final state now runs the exit actions
  of the final state and the root, as XState v5 does (`e_root, e_done, x_done, x_root`).

### Changed

- Conformance: the Python engine converged on XState v5 for the eight recorded divergences
  (Python branch `xstate-v5-engine-parity`); `conformance/divergences.generated.md` is empty and
  cases 020, 021, 027–032 now carry identical Python and XState expectations.
- Conformance: eight new cases (`038`–`045`) pin the remaining engine differences the Python
  engine has now converged on -- conflict preemption and exit/actions/entry grouping in one
  microstep, `done.state.*` on the internal queue, guard evaluation order, `assign` ordering, a
  compound self-target without `reenter`, and a JSON `assign` without the `assignment` wrapper.
  The case format gained a `send_batch` step (events queued together before processing) and
  `logic.record_guards`, which adds the guard evaluations to every observation (`guards`); the
  XState oracle, the Python generator, the schema and `xstate-conformance` (`Step::SendBatch`,
  `Logic::record_guards`, `Observation::guards`, `runner::stub_logic_recording`) support both.
  The Rust engine already matched XState on all eight. The Mermaid / PlantUML goldens now include
  `031_multi_target_parallel_entry`, which the Python library renders since it accepts
  multi-target transitions.

## [0.1.0] - 2026-11-02

First release: a Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine) 0.12.0 with an XState v5
compatible engine, Python-compatible snapshots and store layouts, and the feature set described
below. Developed in phases 0 to 7; the headings follow them.

### Added

#### Phase 0: design and foundations

- Feasibility report and five ADRs: spec-faithful engine semantics with XState v5 as the oracle
  (ADR-0001), a two-tier dynamic / typed context and event model (ADR-0002), byte-compatible
  snapshots and wire formats with the Python library (ADR-0003), a sans-I/O step engine shared by
  every interpreter (ADR-0004) and the workspace and quality gates (ADR-0005).
- Cargo workspace (edition 2024, MSRV 1.88, `unsafe_code = "forbid"`, clippy pedantic,
  `missing_docs`), CI (rustfmt, clippy, tests on three operating systems, MSRV, rustdoc,
  cargo-deny, cargo-audit, coverage, conformance fixture freshness) and the benchmark budget
  layout.
- Conformance suite: JSON fixtures run through two oracles, the real XState 5.33 engine under Node
  and the Python library, recording both observations plus Python's `machine_hash` and snapshot
  vectors. Divergences between the oracles are documented and generated.

#### Phase 1: the engine (`xstate-core`, `xstate-tokio`)

- Lenient XState v5 JSON parser (shorthands, `cond` / `internal` v4 aliases, forbidden
  transitions, inferred `initial`, unknown-key warnings or errors under `strictConfig`, editor
  keys `types` / `schemas` accepted), machine compiler with resolved targets, composite guards and
  every built-in action (`assign`, `raise`, `sendTo`, `sendParent`, `forwardTo`, `escalate`,
  `log`, `cancel`, `stopChild`, `spawnChild`, `emit`, `choose`).
- Sans-I/O step engine transcribed from XState v5 (conflict preemption, exit and entry sets,
  history, `onDone` propagation, eventless settling with `maxIterations`, action and guard error
  policies (`actionErrorPolicy` defaults to `fail`, as in XState), `onUnhandled` ignore / defer / error) and the synchronous interpreter with a timer
  heap and `Clock` / `SimulatedClock`.
- Snapshots in the Python layout v4 (same fields, order and values; compact JSON where Python's
  `get_snapshot()` indents, pinned by a text comparison against Python's output) and the machine
  structure hash, byte-for-byte; restore with hash verification and timer restart modes
  (parked / restart / resume / fire-due; `RestoreOptions::default()` resumes, where Python's bare
  `from_snapshot` parks).
- `xstate-tokio`: `spawn` and the cloneable `Interpreter` handle, bounded mailbox with
  Block / DropNewest / Reject overflow, `after` timers on `tokio::time`, async services, child
  machine actors, `sendTo` / `sendParent` routing, `systemId` registry, watch-based `view()`.
- Builder API, pure API (`initial_transition`, `transition`), exporters (`to_json`, Mermaid and
  PlantUML, byte-identical to Python on the fixture machines), graph utilities (`reachable_states`,
  `shortest_paths`, `simple_paths`, coverage targets, `stub_logic`) and the `Observer` trait.
- Engine benchmarks and the first performance budgets.

#### Phase 2: persistence (`xstate-persistence`)

- `StateStore` contract with memory, file (`.xsm.json` records, atomic rename, advisory locks) and
  SQLite stores sharing the Python layouts; cross-language tests in both directions.
- Optimistic and pessimistic locking, the `persisted` transaction wrapper with retries and
  after-commit callbacks, durable `after` deadlines and `DueTimerScanner`, the idempotency inbox
  and receipts, the transition / audit log, snapshot migrations and `adopt`.

#### Phase 3: observability, inspector and CLI (`xstate-observability`, `xstate-inspect`, `xsm`)

- `MetricsObserver` over the `metrics` facade with the Python `PrometheusPlugin` metric set and
  label hygiene (declared events only, capped cardinality).
- Stately Inspector protocol (`@statelyai/inspect` 0.7.2): `InspectorObserver` with deny-by-default
  context and payload allow-lists and key redaction, memory and JSONL sinks, paced replay;
  recordings are byte-identical to the Python `InspectorPlugin`.
- `xsm` CLI: `validate`, `inspect`, `diagram`, `docs`, `simulate` (scripts, guard overrides,
  virtual clock, `--record`), `paths`, `snapshots`, `info`, with `--json` output and exit codes
  0 / 1 / 2.

#### Phase 4: event-driven architecture and patterns (`xstate-eda`, `xstate-patterns`)

- CloudEvents 1.0 `Envelope` (byte-identical JSON, UUIDv7 ids), the `BrokerAdapter` /
  `SyncBrokerAdapter` contract with fake brokers, `InboundDispatcher` (per-subject order, inbox
  dedup, bounded retries, poison messages to dead letters, replay), the transactional outbox with
  memory and SQLite stores and a relay with claim leases, dead-letter stores and plugin, AsyncAPI
  3.0 generation.
- `RetryPolicy`, `CircuitBreaker`, `SagaBuilder` and `ChoreographyRouter`, emitting the same chart
  JSON as the Python builders.

#### Phase 5: typed DSL, HTTP and Redis (`xstate-macros`, `xstate-axum`)

- `#[derive(TypedEvent)]`, `#[derive(TypedState)]` and the compile-time checked `statechart!`
  DSL, with `trybuild` diagnostics tests.
- `xstate-axum`: `StatechartRegistry`, REST and per-event routes, SSE and WebSocket streaming,
  RFC 9457 problem details, idempotency keys, authorizers, probes, graceful drain and the
  token-gated Stately Inspector WebSocket endpoint.
- Redis backends: `RedisStore`, `RedisInbox`, `RedisLog` (feature `redis` of `xstate-persistence`)
  and `RedisStreamsBroker` (feature `redis-streams` of `xstate-eda`) with the Python key layout
  and Lua compare-and-set scripts.

#### Phase 6: PostgreSQL, more brokers, live inspector, agents (`xstate-agents`)

- PostgreSQL backends over sqlx (feature `postgres`): `PostgresStore`, `PostgresInbox`,
  `PostgresLog` with the Python SQLAlchemy schema, lease-row locks and compare-and-set saves.
- `NatsBroker` (JetStream, durable pull consumers, `Nats-Msg-Id` dedup) and `RabbitMqBroker`
  (durable queues, publisher confirms), async and blocking.
- `SqsBroker` (feature `sqs` of `xstate-eda`, exposed by the facade as `sqs`): the Amazon SQS
  adapter (queue per topic, FIFO groups and deduplication ids, long polling, visibility-timeout
  redelivery), wire-compatible with the Python library's `contrib.brokers.sqs`. Tested against
  ElasticMQ through `XSM_SQS_ENDPOINT`.
- `SseSink`, a standard-library-only hardened HTTP/SSE server for the Stately Inspector page, and
  `xsm inspect --live` / `xsm replay`.
- `xstate-agents`: the tool-loop, supervisor, pipeline and debate charts, `AgentLogic` with
  allow-lists, schema checks and approvals, budgets as guards, tool registry, runner with
  snapshot resume, trace observer, structured output and multi-agent helpers; six Python golden
  scenarios match exactly.

#### Phase 7: release preparation (`xstate-statemachine`)

- The `xstate-statemachine` facade crate: one dependency re-exporting every library crate behind
  feature flags that mirror the Python extras (`tokio`, `persistence`, `sqlite`, `redis`,
  `postgres`, `eda`, `redis-streams`, `nats`, `rabbitmq`, `sqs`, `patterns`, `observability`,
  `inspect`, `macros`, `axum`, `agents`, `testing`, `full`) and a `prelude`.
- README for every crate, a refreshed root README with a feature-parity matrix, and the guide in
  `docs/guide/`: getting started, the Stately editor, persistence, event-driven architecture,
  migrating from Python and the feature-flag reference.
- CI: an `integration` job against Redis, PostgreSQL, NATS (JetStream), RabbitMQ and ElasticMQ, a
  benchmark build job, and cargo-deny over the whole workspace; a `release.yml` workflow with a
  tag check, the gates, a dry-run publish in dependency order and a gated real publish;
  `scripts/publish-order.txt` and `scripts/publish.sh`.
- Workspace dependencies on `xstate-persistence` and `xstate-eda` are declared with
  `default-features = false` (cargo 1.88 rejects a member that disables defaults the workspace
  entry enables); `xsm` enables `sqlite` itself. The crates keep `sqlite` as their own default.
- The CLI package is `xstate-statemachine-cli` because the crate name `xsm` is already taken on
  crates.io; the installed binary is still `xsm`.
- `xstate-py`: Python bindings (PyO3) backed by the Rust engine. Work in progress; not published
  to crates.io (`publish = false`); CI builds it with maturin and runs its pytest suite.

### Known limitations

- MSRV is 1.88. The `postgres` feature (and `full`, which includes it) needs Rust 1.94 (`sqlx`
  0.9); the `MSRV` CI job checks the workspace and every other feature on 1.88 and skips
  `postgres`.
- The outbox is at-least-once and has memory and SQLite stores only (no PostgreSQL outbox).

### Not yet supported

- Kafka broker adapter.
- `xsm` commands `coverage`, `asyncapi`, `dlq`, `generate-template`, `list-templates`, `new`,
  `plugins`, `update` and `setup` of the Python CLI.
- OpenTelemetry and structlog / Sentry observers (`xstate-observability` covers the Prometheus
  metric set through the `metrics` facade).

### Fixed

- `serde_json` `float_roundtrip` is enabled workspace-wide so wall-clock deadlines survive a JSON
  round trip bit for bit.

### Security

- Dependency tree cleaned for `cargo deny` (advisories, licenses, bans, sources): the AWS SDK now
  uses its `default-https-client` (hyper 1 and rustls 0.23) instead of the legacy `rustls`
  feature, which removes `h2` 0.3, `rustls` 0.21 and `rustls-webpki` 0.101 and their advisories;
  `async-nats` is updated to 0.50 (`rustls-webpki` 0.103). One informational advisory
  (RUSTSEC-2025-0134, `rustls-pemfile` unmaintained, reached through lapin 3) is ignored with the
  reason recorded in `deny.toml`.

[Unreleased]: https://github.com/basiltt/xstate-statemachine-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/basiltt/xstate-statemachine-rs/releases/tag/v0.1.0
