# §6.5 Python parity audit

**Scope.** A module-by-module walk of the Python package
`C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine\src\xstate_statemachine\`
(version **0.12.1**, the released version the venv has) mapped to the Rust workspace crates. The
Python reference was read only; the Rust source/tests were read only. No Python or Rust code was
modified, no broad builds were run (the session's full suite is running; only the Python venv was
exercised, with `-I`, for cheap behavioural spot-checks).

**Legend.**

| Verdict | Meaning |
|---|---|
| **same** | Ported with equivalent behaviour, pinned by a Rust test, a Python-generated golden, or a conformance case |
| **documented difference** | Ported, but the behaviour or name differs by design and is recorded in `docs/guide/migrating-from-python.md` |
| **missing** | Not ported; "recorded" means the migration guide (`mg`) or README parity table already lists it, "not recorded" means neither does |

Where a Rust item is cited as `crate:file.rs:NN`, the line is the `pub` definition found during this
audit. Rust file paths are relative to `crates/<crate>/src/`.

---

## Verdict summary

| Area | same | documented difference | missing |
|---|---:|---:|---:|
| Engine / core | 14 | 4 | 7 |
| Events / actions / guards | 11 | 2 | 1 |
| Interpreters / clock | 7 | 5 | 6 |
| Snapshots / structure hash | 5 | 1 | 1 |
| Persistence | 20 | 3 | 4 |
| EDA / brokers | 13 | 3 | 2 |
| Patterns | 4 | 2 | 0 |
| Observability | 1 | 1 | 2 |
| Inspector | 6 | 1 | 0 |
| HTTP adapters | 1 | 1 | 7 |
| Agents | 10 | 3 | 5 |
| CLI | 9 | 1 | 10 |
| Pythonic DSL | 1 | 2 | 1 |
| Testing utilities | 5 | 0 | 3 |
| **Totals** | **107** | **29** | **49** |

Every row is a symbol or a small group of the same symbol (e.g. the four store variants count as
one row each only when they diverge). The counts are conservative: they count named public symbols
and public subpackages, not every method of every class.

---

## Engine / core

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `factory.create_machine(config, logic=, logic_modules=, logic_providers=, input=, strict_config=, strict_targets=, context_validator=, event_schemas=)` | `Machine::from_json` / `Machine::from_value` / `Machine::from_value_with_logic` (`xstate-core:machine/mod.rs:33`), logic passed to the interpreter | **same** (JSON build + config validation) — `logic_modules` / `logic_providers` / `context_validator` / `event_schemas` are Python-only kwargs | conformance 53 cases; `config/parse/tests.rs` |
| `models.MachineNode` | `Machine` (`xstate-core:machine/mod.rs:33`) | **same** | conformance; `machine/direct_tests.rs` |
| `MachineNode.to_json / to_mermaid / to_plantuml` | `Machine::to_json/to_mermaid/to_plantuml` (`machine/mod.rs:354/367/373`, `export.rs:69/87`) | **same** (Mermaid renames the reserved id `end`; Python emits invalid Mermaid) | `export_golden.py` — 72 Mermaid/PlantUML files byte-identical |
| `MachineNode.structure_hash` | `Machine::structure_hash` (`machine/mod.rs:456`) / `machine_hash` (`snapshot.rs:224`) | **same** (SHA-256, byte-identical) | conformance snapshot-vector tests; verified Python emits 64-hex SHA-256 |
| `MachineNode.version` | machine version read from the JSON root | **same** | conformance; Python spot-check (`version` is `None` when absent) |
| `models.ActionDefinition` | `ActionDef` (`xstate-core:machine`), `Builtin` | **same** | config parser tests |
| `models.event_payload` (invoke `input` sentinel) | — | **missing** (recorded: `mg` §"Semantic differences"/"Model and API"; README "Engine" row) | invoke `input` is static JSON only; no input/output factory |
| `machine_logic.MachineLogic(actions=, guards=, services=, delays=)` | `MachineLogic<C>` (`xstate-core:logic.rs:133`) with `.action/.guard/.service/.async_service/.machine_service/.delay` | **same** (registered-logic model) | `logic.rs` doc-test; pure API test |
| `logic_loader.LogicLoader` (singleton module/provider auto-loader) | — | **missing** (not recorded) | no Rust counterpart; `mg` maps `MachineLogic` only |
| `pythonic.py` + `_pythonic_*` class DSL (`State`, `StateMachine`, `build_machine`, `transition`/`action`/`guard`/`service` decorators) | `statechart!`, `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `MachineBuilder` (`builder.rs:404`) | **documented difference** (Rust-native DSL; metaclass scanning and naming-convention hooks not copied) | `mg` API row; README "Typed DSL" row |
| `_typing.py` (`TypedEvent`, `TypedState`) | `typed.rs`; derive macros `xstate-macros:typed_event.rs`, `typed_state.rs` | **same** (Rust-native) | macro UI/compile tests |
| `context_keys.py` (`public_context`, `is_private_context_key`, `PRIVATE_CONTEXT_PREFIX`) | — | **missing** (not recorded) | no match in `xstate-core` (grep found none); distinct from `redact.rs` denylist |
| `coverage.CoverageCollector` (runtime hit collector) | static `transition_coverage_targets` / `state_coverage_targets` only (`graph/targets.rs:42/65`) | **missing** (not recorded; README "Testing helpers" says "coverage targets", which is the static list, not the runtime collector) | no `CoverageCollector` observer in Rust |
| `deprecations.py`, `resolver.py`, `validation.py` (graph walk helpers), `task_manager.py`, `emit.py` (core) | internal helpers; no public Rust equivalent needed | **missing** (internal, not recorded) | N/A |

---

## Events / actions / guards

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `events.Event`, `StreamEvent`, `AfterEvent`, `DoneEvent`, `ErrorEvent` | `Event` (`xstate-core:event.rs:38`) + engine event spellings | **documented difference** (v4 spellings `after.*`, `done.state.*`, `done.invoke.*`, `error.platform.*` in snapshots/hash; engine in-memory spellings are XState v5) | `mg` §"Semantic differences" first table; ADR-0001/0003 |
| `events.is_system_event`, `system_event`, `re_mint`, `SYSTEM_EVENT_PREFIXES`, `ENGINE_EVENT_SHAPES` | engine/system event construction internal to `xstate-core` | **same** (behaviour, not a public API) | conformance wildcard/system-event cases |
| `events.Receipt` | `Receipt` (`xstate-core:interpreter.rs:29`) + `StepReport` (`engine/mod.rs:305`) | **documented difference** (fields differ; see note below) | `mg` errors row; axum `receipt.rs` |
| `receipts.receipt_to_json/from_json/to_status` | `xstate-persistence:receipts.rs` (`ReceiptJson`, `receipt_to_status`) | **same** | `receipts.rs` unit tests; Python spot-check (`receipt_to_status` maps `error` → status) |
| `actions.assign, raise_, send_to, send_parent, forward_to, escalate, log, cancel, stop_child, spawn_child, emit, pure, choose, enqueue_actions, ActionEnqueuer` | JSON-form built-ins parsed in `machine/builtin.rs`; executed in `engine/actions.rs` | **same** | conformance 53 cases exercise every built-in; `engine/actions.rs` tests |
| guards `and / or / not / stateIn` | `Guard::And/Or/Not/StateIn` (config parse) | **same** | conformance cases `038`–`045` (guard order, composite guards) |
| `actor_logic.from_callback / from_iterator / from_async_iterator / from_coroutine / from_callable / from_interpreter` | tokio services registered as closures / machine services via `MachineLogic::service/async_service/machine_service` | **documented difference** (no named `from_*` creators; the tokio interpreter drives closures and child machines) | `mg` async-actors row; tokio actor tests |
| `actor_logic.RunningLogic`, `drain_pending_cleanups`, `DEFAULT_CLEANUP_TIMEOUT` | tokio cleanup internals | **missing** (internal; not recorded) | N/A |
| `models.OverflowPolicy` | `Overflow` (`xstate-tokio:lib.rs:38`) | **same** | tokio mailbox tests |
| `models.event_payload` | — | **missing** (see Engine) | `mg` |

**Receipt field note.** Python `Receipt` = `state_ids` (frozenset), `changed`, `error`, `deferred`,
`denied`, `duplicate`. Rust `Receipt` = `state_ids()` (sorted Vec), `changed`, `unhandled`,
`actions`, `status`, `error`. The Rust side has **no** `deferred`, `denied` or `duplicate` flags;
`unhandled` ("no transition matched") is not the same as Python's `denied` ("handler existed, every
guard false"). This difference is **not recorded** in `mg` (only the `ErrorKind`-vs-exception and
HTTP-status-mapping differences are).

---

## Interpreters / clock

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `sync_interpreter.SyncInterpreter(machine).start()` | `SyncInterpreter::new(machine, logic).start()` (`xstate-core:interpreter.rs:128/216`) | **same** (core lifecycle: `start/send/tick/stop/status`) | `xstate-core` interpreter tests; conformance harness |
| `interpreter.Interpreter` (asyncio) | `spawn` / `Interpreter` (`xstate-tokio:lib.rs:121/86`) | **same** (bounded mailbox, overflow, child machines, `sendTo`/`sendParent`) | tokio actor tests |
| `base_interpreter.BaseInterpreter` | no single base; `xstate-core` (sans-I/O) + `xstate-tokio` split | **documented difference** (ADR-0004, one algorithm two drivers) | `mg` §"Model and API" |
| `send_threadsafe`, `WrongThreadError` | — (compile-time `Send + Sync` instead) | **documented difference** | `mg` §"Model and API" |
| `use(plugin)`, `subscribe`, `on`, `PluginBase` (30 hooks) | `Observer` trait (`xstate-core:observer.rs:17`) + `observe` (`interpreter.rs:201`); tokio `subscribe` → `watch::Receiver` (`tokio:lib.rs:157`) | **documented difference** (observer, no entry-point discovery) | `mg` §"Model and API"; observer tests |
| `can()`, `value`, `tags`, `has_tag`, `get_meta`, `queue_depth`, `last_error`, `chain_trips`, `clear_chain_error` | `matches` (`interpreter.rs:377`), `state_ids`, `status`, `context` | **missing** (partial: `matches`/`state_ids`/`status`/`context` cover some; the rest are absent and not recorded) | grep of `interpreter.rs` found no `can`/`has_tag`/`value`/`tags`/`queue_depth`/`last_error` |
| `send_events`, `await_settled`, `wait_done` | tokio `wait_done` (`tokio:lib.rs:222`); `send_events`/`await_settled` absent | **documented difference** (tokio model) / partial missing | `mg`; tokio tests |
| `clock.Clock`, `RealClock`, `SimulatedClock` | `Clock` trait, `RealClock`, `SimulatedClock` (`xstate-core:clock.rs:9/24/54`) | **same** | S7 benchmark; `advance` (`interpreter.rs:320`) |
| tokio pluggable clock | — | **documented difference** (tokio interpreter uses wall clock only) | `mg` §"Configuration checks… snapshot time on tokio" |
| `helpers` pure API `initial_transition`, `transition` (`pure_transition`) | `pure::initial_transition` / `pure::transition` (`xstate-core:pure.rs:33/47`) | **same** (guards evaluated, `assign` applied, actions planned) | `pure.rs` mod test `plans_actions_without_running_them` |
| `helpers.PureSnapshot`, `get_initial_snapshot`, `get_next_snapshot` | `ActiveState<C>` (returned by `pure::*`) | **missing** (the reducer exists, the `PureSnapshot` wrapper type and the two convenience wrappers do not; not recorded) | grep: no `PureSnapshot`/`get_*_snapshot` in Rust |
| `helpers.wait_for`, `wait_for_sync`, `to_promise` | — | **missing** (not recorded) | grep: no `wait_for`/`to_promise` in Rust |

---

## Snapshots / structure hash

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| snapshot layout v4 (`persistence/snapshot.py`: `SNAPSHOT_VERSION`, `check_shape`, `check_version`, `check_minimum_version`, `check_identity`, `upcast`) | `Snapshot` (`xstate-core:snapshot.rs:52`), `Snapshot::from_json`, restore (`interpreter/restore.rs`) | **same** (layout v4, same fields/order/values; Rust writes compact JSON) | conformance snapshot vectors; Python spot-check of `get_snapshot()` field list matches (`actors`, `configuration`, `context`, `deadlines`, `deferred`, `history`, `machine_hash`, `machine_id`, `machine_version`, `state_ids`, `status`, `system`, `taken_at`, `value`, `version`, …) |
| `structure_hash` | `Machine::structure_hash` / `machine_hash` | **same** | conformance hash vector; byte-identical |
| `get_snapshot` / `get_persisted_snapshot` | `SyncInterpreter::snapshot` / `snapshot_json` (`interpreter.rs`) | **same** (compact vs indented) | `mg` "What carries over" |
| `from_snapshot` / `from_state` | `SyncInterpreter::from_snapshot(..)` + `RestoreOptions` | **documented difference** (restore resumes timers by default vs Python parks) | `mg` §"Model and API" M7 |
| `persistence.snapshot` `Deadline`, `check_deadline_record` | deadline records on `Snapshot`; `DueTimerScanner` validation | **same** | timers tests |

---

## Persistence

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `store.StateStore`, `BaseStore`, `StoredSnapshot`, `SnapshotCodec`, `validate_key`, `MAX_KEY_LENGTH`, `DEFAULT_MAX_SNAPSHOT_BYTES` | `StateStore` (`xstate-persistence:store.rs:142`) + codec/limits | **same** | store contract tests; cross-language tests |
| `store.MemoryStore` | `MemoryStore` (`memory_store.rs:298`) | **same** | store battle suite |
| `file_store.FileStore` | `FileStore` (`file_store.rs:461`), `FileStoreOptions` (`file_store.rs:277`) | **same** (incl. the 0.12.1 lock-stamp freshness skip) | `e194300` + file-store tests |
| `sqlite_store.SQLiteStore` | `SqliteStore` (`sqlite_store.rs:354`), `SqliteStoreOptions` (`sqlite_store.rs:81`) | **same** (SQLite schema v2; `JournalMode` restricted to `Wal`/`Delete`) | cross-language SQLite tests; DDL-equivalence test |
| `async_store.AsyncStateStore`, `AsyncStoreAdapter`, `as_async` | — | **missing** (recorded: `mg` §"Model and API" — stores are synchronous) | no async store trait |
| `locking.OptimisticLock`, `PessimisticLock`, `NoLock`, `LockStrategy` | `OptimisticLock` (`locking.rs:205`), `PessimisticLock` (`locking.rs:242`), `NoLock` (`locking.rs:268`) | **same** | lock tests; cross-language lock tests |
| `locking.persisted`, `persisted_retry` | `persisted` / `persisted_retry` (`locking.rs:650/671`) | **same** (closure not context-manager) | `mg`; persisted tests |
| `locking.apersisted`, `DEFAULT_SETTLE_TIMEOUT` | — (`apersisted` only) | **missing** (recorded via the async-stores row) | async API not exposed |
| `helpers.load_interpreter`, `save_interpreter`, `KeyNotFoundError` | `load_interpreter` / `save_interpreter` (`locking.rs:426/490`) | **same**; `aload_interpreter` missing | `mg`; `lib.rs:38` re-export |
| `idempotency.InboxStore`, `InboxEntry`, `MemoryInbox`, `SQLiteInbox` | `InboxStore`, `MemoryInbox` (`idempotency/memory.rs:11`), `SqliteInbox` (`idempotency/sqlite.rs:81`), `RedisInbox`, `PostgresInbox` | **same** | inbox contract tests; cross-language |
| `idempotency.default_key`, `fingerprint`, `DEFAULT_TTL_S` | `default_key` / `fingerprint` (`idempotency.rs:129/143`), `DEFAULT_TTL_S` | **same** | idempotency unit tests |
| `idempotency.IdempotencyPlugin` | `IdempotencyGuard` (`idempotency/guard.rs:183`) | **documented difference** — **name is wrong in `mg`**: Python's class is `IdempotencyPlugin`, Rust's is `IdempotencyGuard` ("the decision logic … without the plugin plumbing", per its own doc). `mg` lists it as "the same names". | `guard.rs:181`; Python `idempotency.py` |
| `log.TransitionLogStore`, `TransitionRecord`, `MemoryLog`, `JSONLinesLog`, `SQLiteLog` | `MemoryLog` (`log/memory.rs:12`), `JsonLinesLog` (`log/jsonl.rs:47`), `SqliteLog` (`log/sqlite.rs:28`), `RedisLog`, `PostgresLog` | **same** | log tests; cross-language |
| `log.TransitionLogPlugin`, `AuditPlugin`, `replay`, `correlation_id_var` | `TransitionLogObserver` (`log/observer.rs:89`) | **documented difference** (plugin → observer; `AuditPlugin`/`replay`/`correlation_id_var` have no separate Rust public item — `correlation_id` rides on the record) | `mg` plugins→observers row |
| `timers.DueTimerScanner`, `ScanResult` | `DueTimerScanner` (`timers.rs:223`), `ScanResult`/`ScanSummary` | **same** (zero-interval refusal, `validate`/`last_result` new) | timers battle tests |
| `migration.SnapshotMigrator`, `MigrationStep`, `MachineVersionMismatchError`, `NoMigrationPathError` | `SnapshotMigrator` (`migration.rs:101`), `MigrationError` | **same** | migration tests |
| `adopt.from_state_ids` | `from_state_ids` (`adopt.rs`) | **same** — **name note**: `mg` lists this as "`adopt` … the same names"; both libraries name the function `from_state_ids` (the module is `adopt`), so the guide's symbol name is imprecise, not the mapping. | `adopt.rs` doc "Ports Python `persistence/adopt.py`" |
| `contrib.redis` (`store`, `inbox`, `log`) | `RedisStore` (`redis_backend/store.rs:106`), `RedisInbox`, `RedisLog` | **same** (feature `redis`) | cross-language Redis tests |
| `contrib.sqlalchemy` (`store`) | `PostgresStore` (`postgres_backend/store.rs:157`) | **documented difference** (PostgreSQL only; no MySQL/Django ORM stores) | `mg`; README stores row |

---

## EDA / brokers

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `eda.envelope.Envelope` | `Envelope` / `EnvelopeBuilder` (`xstate-eda:envelope/mod.rs:73/126`) | **same** (CloudEvents 1.0 + `xsmattempt`) | `eda_golden.py` (24 error cases); Python spot-check: `Envelope.new(type=…).to_dict()` → `datacontenttype,id,source,specversion,time,type` |
| `eda.broker` `BrokerAdapter` / sync broker | `BrokerAdapter` / `SyncBrokerAdapter` (`xstate-eda:broker.rs:39/67`) | **same** | broker contract tests |
| `eda.fake.FakeBroker` | `FakeBrokerAdapter` (`fake/adapters.rs:25`) | **same** | dispatcher/outbox tests |
| `eda.dispatcher.InboundDispatcher` | `InboundDispatcher` (`dispatcher/mod.rs:272`) | **documented difference** (async `run_once` batches on scoped threads, acks per batch) | `mg` M8; `dispatcher/consume.rs` |
| `eda.outbox` `OutboxPlugin`, `OutboxRelay`, outbox stores | `OutboxPlugin` (`outbox/plugin.rs:97`), `OutboxRelay` (`outbox/relay.rs:25`), `MemoryOutboxStore` (`outbox/memory.rs:19`), `SqliteOutboxStore` (`outbox/sqlite.rs:46`) | **documented difference** (at-least-once, no PostgreSQL outbox store) | `mg`/README; `eda_golden.py` writes SQLite outbox files read by Rust |
| `eda.dead_letter` `DeadLetterPlugin`, stores, replay | `DeadLetterPlugin` (`dead_letter/plugin.rs:40`), `MemoryDeadLetterStore`/`SqliteDeadLetterStore` (`dead_letter/memory.rs:73`/`sqlite.rs:55`), `replay_file` | **same** (error chain kept in memory per plugin vs Python context — §5) | `eda_golden.py` DLQ files |
| `eda.asyncapi` `asyncapi_document` | `asyncapi_document` (`asyncapi/mod.rs:221`) | **same** (AsyncAPI 3.0) | `eda_golden.py`; `asyncapi/validate.rs` (structural) |
| brokers `contrib.brokers.redis_streams`, `nats`, `rabbitmq`, `sqs` | `RedisStreamsBroker` (`redis_streams/mod.rs:250`), `NatsBroker` (`nats/mod.rs:214`), `RabbitMqBroker` (`rabbitmq/mod.rs:232`), `SqsBroker` (`sqs/mod.rs:253`) | **same** (features `redis-streams`, `nats`, `rabbitmq`, `sqs`) | cross-language broker tests (Redis/NATS/RabbitMQ/SQS) |
| `contrib.brokers.kafka` | — | **missing** (recorded: §5; README "Brokers" row) | no pure-Rust client with consumer groups |
| `contrib.cloudevents` (SDK bridge) | — | **missing** (recorded: `mg` "Not ported") | CloudEvents SDK bridge |

---

## Patterns

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `patterns.retry.RetryPolicy` (jitter modes) | `RetryPolicy` (`xstate-patterns:retry.rs:78`) | **same** (decorrelated jitter context key trails Python by one delay — §5) | `patterns_golden.py` retry delays |
| `patterns.circuit_breaker.CircuitBreaker` | `CircuitBreaker` (`circuit_breaker.rs:353`) + `CircuitBreakerBuilder` (`:206`) | **documented difference** (applied with `wrap`, no decorator) | `mg`; `patterns_golden.py` breaker chart |
| `patterns.saga.SagaBuilder` | `SagaBuilder` (`saga.rs:187`) | **same** (identical chart JSON) | `patterns_golden.py` (3 sagas) |
| `patterns.choreography.ChoreographyRouter` | `ChoreographyRouter` (`choreography.rs:81`) | **documented difference** (sync/blocking brokers only) | `mg`; §5 |
| `patterns.dead_letter` | (`xstate-eda` dead-letter) | **same** | eda golden |

---

## Observability

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `contrib.observability.prometheus.PrometheusPlugin` | `MetricsObserver` (`xstate-observability:metrics_observer.rs:50`) | **same** (metric set over the `metrics` facade; `LabelGuard` cardinality hygiene) | `metric_names.rs`; §6.2 matrix pending |
| `contrib.observability._hygiene.LabelGuard`, `event_label`, `OTHER`, `UNKNOWN` | `hygiene.rs` | **same** | hygiene tests |
| `contrib.observability.otel.OpenTelemetryPlugin`, `agent_span_exporter`, `instrument_all/uninstrument_all` | — | **missing** (recorded: `mg` "Not ported"; README "Observability" row) | |
| `contrib.observability.logs` (structlog/loguru), `sentry.SentryPlugin` | — | **missing** (recorded: `mg` "Not ported") | `TracingObserver` is the Python `LoggingInspector`, not structlog/Sentry |

---

## Inspector

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `inspect.plugin.InspectorPlugin`, `session_id_of` | `InspectorObserver` (`xstate-inspect:observer.rs:44`) | **same** (`@statelyai/inspect` 0.7.2 protocol) | `inspect_golden.py` — 3 recordings byte-identical |
| `inspect.protocol` `MESSAGE_TYPES`, version | `MESSAGE_TYPES`, `PROTOCOL_VERSION` (`xstate-inspect:protocol.rs`, re-export `lib.rs:40`) | **same** | protocol tests |
| `inspect.sinks` `MemorySink`, `JsonLinesSink`, `SseSink` | `MemorySink` (`sinks.rs:62`), `JsonLinesSink` (`sinks.rs:150`), `SseSink` (`sse/mod.rs:137`) | **same** (SSE hardened, std-only HTTP) | inspect golden + SSE security suite (24 tests) |
| `inspect.replay` `read_jsonl`, `replay_messages` | `read_jsonl` (`sinks.rs`), `replay_file`/`replay_with` (`replay.rs`) | **same** | replay tests |
| `inspect._page` (inspector HTML page) | `sse/page.rs` | **documented difference** (forwards to stately.ai when "Stately UI" ticked, default on) | §5; `sse/page.rs` |

---

## HTTP adapters

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `contrib.starlette.registry.StatechartRegistry` + `streaming` + `inspector` | `StatechartRegistry` (`xstate-axum:registry.rs:360`), REST + per-event routes, SSE/WebSocket, probes, inspector WS | **documented difference** (axum is the Rust-native equivalent, not a port) | `mg`; axum route/WS tests; `60-alignment.md` (origin/redaction fixes) |
| `contrib.fastapi` (`StatechartRouter`) | `xstate-axum` | **documented difference** (mapped to axum) | `mg` API row |
| `contrib.django` (models, mixin, stores, admin, fsm, signals, management commands, deadlines) | — | **missing** (recorded: §6.5, `mg` "Not ported") | |
| `contrib.drf` (fields, permissions, viewsets) | — | **missing** (recorded) | |
| `contrib.flask` (`_core`, `_http`, blueprint, extension, cli, session_store) | — | **missing** (recorded) | |
| `contrib.quart` | — | **missing** (recorded) | |
| `contrib.litestar` (controller, plugin, edge) | — | **missing** (recorded) | |
| `contrib.channels` (broadcast, consumer) | — | **missing** (recorded: `mg` "Not ported") | |
| `contrib.celery` (beat, service, worker) | — | **missing** (recorded: §6.5) | |
| `contrib._openapi`, `contrib._registry` | absorbed by axum registry/problem-types | **missing** (internal; not recorded) | N/A |

---

## Agents

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `contrib.agents.core` (`agent_logic`, `TOOL_LOOP`, `load_chart`, `validate_agent_chart`) | `AgentLogic` (`xstate-agents:logic/mod.rs:51`), `TOOL_LOOP_JSON`/`load_chart` (`charts.rs`), `validate_agent_chart` | **same** (TOOL_LOOP/supervisor/pipeline/debate charts) | `agents_golden.py` (6 scenarios) |
| `tools` (`Tool`, `ToolCall`, `ToolRegistry`, `tool_registry`, `tool`, `ALL_TOOLS`) | `ToolRegistry`/`ToolRegistryBuilder` (`tools/registry.rs:23/74`) | **same** | agents golden; tool schema tests |
| `budgets` (`Budget`, `budget_guards`, `check_budget_event_declared`) | `Budget`/`budget_guards` (`budgets.rs`) | **same** | agents golden (budget guards) |
| `multi` (`spawn_agent`, `handoff_guard`) | `spawn_agent` (`multi.rs:124`), `handoff_guard` | **documented difference** (sub-agents run to completion inside the parent's action) | §5; `mg` |
| `runner` (`run_agent`, `run_agent_sync`) | `runner.rs` (`run_agent`/`run_agent_sync`) | **same** | agents golden |
| `structured` (`structured_output`, `validate_structured`) | `structured_output`/`validate_structured` (`structured.rs`) | **same** (JSON-schema output) | agents golden |
| `trace` (`AgentTracePlugin`) | `AgentTrace`/`TraceSink` (`trace.rs`) | **same** | agents golden (trace records) |
| `messages` (`ModelCall`, `ModelResponse`, `FakeModel`, `Usage`) | `AgentLogic` call-model closure (`logic/call_model.rs:29`) | **documented difference** (no `FakeModel`/`ModelCall`/`ModelResponse`/`Usage` public types; the model is a closure) | §5 |
| `_allowlist` (`state_tools`), `_output`, `_threads` | `allowlist.rs` (`state_tools`), `output.rs` | **same** | agents golden |
| `providers.anthropic`, `providers.openai` | — | **missing** (recorded: §5; `mg` "provider adapters … not ported") | |
| `pydantic_ai`, `langgraph` | — | **missing** (recorded: §5; `mg`) | |

---

## CLI

Python command inventory (from `cli/args.py`): `validate`, `paths`, `coverage`, `info`, `plugins`,
`update`, `snapshots`, `setup`, `inspect`, `replay`, `diagram`, `simulate`, `docs`, `new`, `dlq`,
`asyncapi`, `generate-template`, `list-templates`, plus the interactive launcher/TUI
(`cli/commands/launcher.py`, `cli/ui/*`).

| Python command | Rust (`crates/xsm`) | Verdict | Evidence |
|---|---|---|---|
| `validate`, `inspect` (`--live`), `replay`, `diagram`, `docs`, `simulate`, `paths`, `snapshots`, `info` | `commands/{validate,inspect,replay,diagram,docs,simulate,paths,snapshots,info}.rs` (`main.rs:59-82`) | **same** (9 commands) | `xsm/tests/`; `mg` CLI subset row |
| `snapshots --stale` label comparison / `--stale-by hash` | `commands/snapshots.rs` | **documented difference** (skips records naming another machine id; extra `hash_mismatch` column) | `mg` §"CLI subset" |
| `coverage`, `asyncapi`, `dlq`, `generate-template`, `list-templates`, `new`, `plugins`, `update`, `setup` | — | **missing** (recorded: `mg`/README list all nine) | |
| interactive launcher / TUI (`launcher.py`, `ui/animate.py`, live wizard) | — | **missing** (**not recorded**: `mg` and the README CLI row both omit the interactive TUI; §6.5 lists it) | Rust CLI has no no-argument interactive path |

---

## Pythonic DSL, testing utilities, framework glue

| Python symbol / module | Rust equivalent | Verdict | Evidence |
|---|---|---|---|
| `pythonic.StateMachine` / `State` class DSL | `statechart!` macro | **documented difference** (Rust-native) | `mg`; README |
| `testing_utils.stub_logic` | `stub_logic` (`xstate-core:graph/stub.rs:132`) | **same** (constant guards, no-op actions, `null` services, 0 ms delays) | `graph/stub.rs` doc-test |
| `testing_utils.logic_names` | — (`collect_names` is `pub(crate)` only) | **missing** (**not recorded**: `mg` maps `stub_logic` but not `logic_names`) | `graph/stub.rs:74` |
| `graph.reachable_states`, `shortest_paths`, `simple_paths`, `transition_coverage_targets`, `Path`, `Step` | `reachable_states`/`shortest_paths`/`simple_paths` (`graph.rs:237/277/298`), `transition_coverage_targets` (`graph/targets.rs:42`), `Path`/`PathStep` (`graph.rs:165/149`) | **same** | `graph.rs` tests |
| `contrib.testing` pytest plugin, `gwt`, `broker`, fixtures, `_paths`, `_snapshots` | `xstate-testing` is a skeleton (`xstate-testing:lib.rs`); graph helpers in `xstate-core::graph` | **missing** (recorded: README "Testing helpers"; pytest plugin listed in `mg` "Not ported") | §5 |
| `contrib.pydantic` (config, context, events, schema, scrub) | — | **missing** (recorded: §6.5; `mg` "pydantic … not ported") | |
| `plugin_discovery.py` (`register_global`, `unregister_global`, `global_plugins`) | — | **missing** (recorded: `mg` "no entry-point discovery") | |

---

## Known gaps (confirming §6.5, plus further findings)

Recorded in §6.5 and confirmed still absent:

1. Kafka broker adapter (`contrib/brokers/kafka.py`) — missing; §5 and README record it.
2. SQLAlchemy outbox store / `ModelStore` / `StatechartMixin` (`contrib/sqlalchemy/outbox.py`,
   `model_store.py`, `mixin.py`) — missing; README stores/outbox rows record "no PostgreSQL outbox"
   and "no … ORM stores", but `ModelStore`/`StatechartMixin` are not named explicitly in `mg`.
3. Celery (`contrib/celery`) — missing; recorded in `mg` "Not ported".
4. Django / DRF / Flask / Quart / Litestar / Starlette / FastAPI beyond axum — missing; Starlette and
   FastAPI are mapped to axum (`mg`), the rest recorded as not ported.
5. pydantic models (`contrib/pydantic`) — missing; recorded.
6. Agent provider integrations (`providers/anthropic`, `providers/openai`, `pydantic_ai`,
   `langgraph`) — missing; recorded in §5 and `mg`.
7. CLI `list-templates` (and `coverage`, `asyncapi`, `dlq`, `generate-template`, `new`, `plugins`,
   `update`, `setup`) — missing; recorded in `mg`/README.
8. Interactive TUI (`cli/commands/launcher.py`, `cli/ui/*`) — missing; **not recorded** in `mg` or
   the README CLI row (only §6.5 names it).

Further gaps found during this walk (not in §6.5's list):

9. `LogicLoader` (singleton module/provider logic auto-loading) — missing, not recorded.
10. `helpers.PureSnapshot`, `get_initial_snapshot`, `get_next_snapshot`, `wait_for`, `wait_for_sync`,
    `to_promise` — missing, not recorded (only the `pure_transition`/`initial_transition` reducers
    are ported).
11. `context_keys` (`public_context`, `is_private_context_key`, `PRIVATE_CONTEXT_PREFIX`) — missing,
    not recorded.
12. `coverage.CoverageCollector` (runtime hit collector) — missing, not recorded (only the static
    coverage targets are ported).
13. `testing_utils.logic_names` — missing, not recorded (`collect_names` is crate-private).
14. Interpreter helpers `can()`, `value`, `tags`, `has_tag`, `get_meta`, `queue_depth`,
    `last_error`, `chain_trips`/`clear_chain_error` — missing, not recorded (partial: `matches`,
    `state_ids`, `status`, `context` cover part of it).
15. `actor_logic` `from_callback`/`from_iterator`/`from_async_iterator`/`from_coroutine`/
    `from_callable`/`from_interpreter` named creators — no direct Rust equivalents (services are
    closures/machines), recorded only obliquely.
16. `receipts` Receipt flags `deferred`, `denied`, `duplicate` — no Rust counterpart, not recorded.

---

## Incorrect or imprecise documentation claims

These are statements in `docs/guide/migrating-from-python.md`, the README parity table or the book
pages that the code contradicts. Severity per §8.

1. **MEDIUM — `mg` line 33 lists `IdempotencyGuard` as "the same names".** Python's class is
   `IdempotencyPlugin` (`persistence/idempotency.py:576`); Rust's is `IdempotencyGuard`
   (`crates/xstate-persistence/src/idempotency/guard.rs:183`), whose own doc says it is "the
   decision logic of the Python `IdempotencyPlugin`, without the plugin plumbing". The mapping is
   right, the "same names" claim is not.
2. **MEDIUM — `mg` line 33 lists `adopt` as "the same names".** Neither library exposes a symbol
   named `adopt`; both expose `from_state_ids` (Python `persistence/adopt.py:118`, Rust
   `crates/xstate-persistence/src/adopt.rs`). The guide should name `from_state_ids`.
3. **LOW — `mg` "Semantic differences" second table says the engine uses v5 event spellings "in
   memory; v4 spellings are accepted on input"** and separately says snapshots/hash use v4. That is
   accurate, but the first-table row "Multi-target transitions in diagrams, graph and coverage —
   only the first target is shown (Python)" is the *Python* column; the Rust column reads "every
   target", which contradicts the §"What carries over" claim that the structure hash renders only
   the first target in both runtimes. The two are not contradictory (hash ≠ diagrams/graph), but the
   pair is easy to misread; consider cross-linking.
4. **LOW — README parity table says "Against `xstate-statemachine` 0.12.0"** while the venv/CI oracle
   is 0.12.1 (the FileStore change was ported and the fixtures regenerated against 0.12.1). The
   version label is stale; say 0.12.1.
5. **LOW — README "Testing helpers" says "coverage targets … in `xstate-core`"** and `mg` line 40
   repeats it; this is true for the static targets only. The runtime `CoverageCollector` is absent
   and the wording ("coverage targets") can be read as the whole coverage feature. Worth one clause.
6. **LOW — `mg` CLI subset row and README CLI row list nine missing commands but omit the
   interactive launcher/TUI**, which the Python CLI also has (bare `xsm` on a TTY, plus
   `ui/animate.py`). §6.5 already names it; the user-facing docs do not.

The one already-fixed class (`invoke.input` factories) is correctly recorded now: `mg` "Semantic
differences" table and "Model and API" both state that input/output factories are not ported, and
the README "Engine" row says the same. No re-fix needed.

---

## Verified (cheap) this session

Run with the Python 0.12.1 venv (`-I`), cross-checked against the Rust sources cited above:

- `machine.structure_hash` is a 64-char hex SHA-256; `machine.version` is `None` when absent.
- `get_snapshot()` (parsed) exposes the layout-v4 field set (`actors`, `chain_trips`,
  `configuration`, `context`, `deadlines`, `deferred`, `error`, `history`, `last_chain_error`,
  `machine_hash`, `machine_id`, `machine_version`, `output`, `pending_events`, `scheduled_sends`,
  `state_ids`, `status`, `system`, `taken_at`, `value`, `version`) — matches `xstate-core` snapshot.
- `Envelope.new(type=…)` is keyword-only with `source="xsm"`; `to_dict()` yields
  `datacontenttype,id,source,specversion,time,type` (no `xsmattempt` until an attempt is set) —
  matches `xstate-eda` envelope.
- `Receipt` is a `NamedTuple` (`state_ids, changed, error, deferred, denied, duplicate`);
  `receipt_to_status` maps `error` → non-2xx status — matches `xstate-persistence` receipts.
- The conformance, golden and cross-language test names cited above were located in the repo (they
  were not re-run: the session's full suite is running and cargo was not invoked).

## Could not verify (within this session's constraints)

- Nothing was compiled or run under cargo; every Rust behavioural claim above is pinned to a cited
  test/golden/conformance case rather than re-executed here.
- Cross-language store/broker tests and the agents/inspect/patterns/export goldens were not re-run
  (they need Docker services or a full build).
- Deep behavioural spot-checks of `contrib/django`, `contrib/celery`, `contrib/channels`,
  `contrib/drf`, `contrib/flask`, `contrib/quart`, `contrib/litestar`, `contrib/pydantic` and
  `contrib/cloudevents` internals were not done — those packages are entire integrations judged
  "missing" as units, so their internal symbols were not itemised.
- The async interpreter method surface (`send_events`, `await_settled`, `subscribe`/`on` on the
  sync interpreter) was assessed by grep of `interpreter.rs`/`lib.rs`, not by compilation.
