# Migrating from the Python library

The Rust crates are a port of [`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine)
0.12.0: same JSON dialect, same snapshot layout, same store layouts, same wire formats, same CLI
shape. A Rust worker can resume an instance a Python worker saved and the other way round, so you
can migrate one service at a time. This page maps the API and collects the places where the
behaviour is **not** the same.

## API mapping

| Python | Rust |
|---|---|
| `create_machine(config, logic=logic)` | `Machine::from_json(text)?` or `Machine::from_value(&value)?`, plus a `MachineLogic` passed to the interpreter |
| `MachineLogic(guards=..., actions=..., services=..., delays=...)` | `MachineLogic::dynamic().guard(name, f).action(name, f).service(name, f).async_service(name, f).machine_service(name, machine).delay(name, ms)` |
| guard `lambda ctx, event: ...` | `\|ctx, event, params\| -> Result<bool>` |
| action `lambda interp, ctx, event, action: ctx.update(...)` | `\|scope\| { scope.context[..] = ..; Ok(()) }`; `scope.event`, `scope.params`, `scope.raise(..)`, `scope.emit(..)` |
| `SyncInterpreter(machine).start()` | `SyncInterpreter::new(machine, logic)` then `.start()?` |
| `Interpreter(machine)` (asyncio), `await interp.start()` | `spawn(machine, logic, Options::default())?` (feature `tokio`), a cloneable handle |
| `interp.send("E")`, `interp.send({"type": "E", ...})` | `interp.send("E")?`, `interp.send(Event::with_payload("E", json!({..})))?`; typed events through `TypedEvent` |
| `interp.current_state_ids` | `interp.state_ids()` |
| `interp.context` | `interp.context()` (`Option<&C>`) |
| `interp.get_snapshot()` / `from_snapshot(...)` | `interp.snapshot()?`, `snapshot_json()?` / `SyncInterpreter::from_snapshot(..)` with `RestoreOptions` |
| `SimulatedClock` | `SimulatedClock` (`interp.advance(&clock, ms)`) |
| `to_json()`, `to_mermaid()`, `to_plantuml()` | `machine.to_json()`, `to_mermaid()`, `to_plantuml()` |
| class DSL (`StateMachine`, `State`, `\|`), builder, functional form | `statechart!`, `#[derive(TypedEvent)]`, `#[derive(TypedState)]` (feature `macros`); `MachineBuilder` in `xstate_core::builder` |
| `PluginBase` subclasses, `plugins=[...]` | the `Observer` trait; `interp.observe(Arc::new(..))` |
| `InspectorPlugin` / `PrometheusPlugin` | `InspectorObserver` (`inspect`) / `MetricsObserver` (`observability`) |
| `persisted(store, key, machine, ...)` context manager | `persisted(&store, key, &machine, &logic, &options, \|interp, session\| ..)` closure |
| `MemoryStore`, `FileStore(dir)`, `SQLiteStore(path)` | `MemoryStore::new()`, `FileStore::open(dir)`, `SqliteStore::open(path)` |
| `contrib.redis` stores | `RedisStore::open(url, &RedisStoreOptions::new(prefix))` (`redis`) |
| `contrib.sqlalchemy` stores | `PostgresStore::connect(url, &PostgresStoreOptions::default())` (`postgres`; PostgreSQL only) |
| `OptimisticLock`, `PessimisticLock` | the same names |
| `DueTimerScanner`, `IdempotencyGuard`, `SnapshotMigrator`, `adopt` | the same names |
| `Envelope.new(type)`, `InboundDispatcher`, `OutboxPlugin`, `OutboxRelay` | `Envelope::new(type)`, `InboundDispatcher::new`, `OutboxPlugin::new`, `OutboxRelay::new` |
| `DeadLetterPlugin`, `asyncapi_document`, `replay_dead_letter` | the same names |
| Redis Streams, NATS, RabbitMQ, SQS brokers | `RedisStreamsBroker`, `NatsBroker`, `RabbitMqBroker`, `SqsBroker` (features `redis-streams`, `nats`, `rabbitmq`, `sqs`); Kafka not yet |
| `RetryPolicy`, `CircuitBreaker`, `SagaBuilder`, `ChoreographyRouter` | the same names (`patterns`); the breaker is applied with `wrap(breaker, f)` (there is no decorator), and `ChoreographyRouter` drives blocking brokers only |
| `contrib.starlette`, `contrib.fastapi` | `xstate-axum`: `StatechartRegistry`, `router`, `inspector_router` |
| `contrib.agents` | `xstate-agents` (`agents`) |
| `testing_utils.stub_logic`, `graph` helpers, coverage | `xstate_core::graph` (`stub_logic`, `reachable_states`, `shortest_paths`, `simple_paths`, coverage targets) |
| `xsm` CLI | `xsm` (a subset of the commands, see the README matrix); `xsm snapshots --stale` compares `machine_version` labels like Python, `--stale-by hash` compares structure hashes |
| exceptions (`ImplementationMissingError`, `InvalidConfigError`, ...) | `xstate_core::Error` and its `ErrorKind`; `StoreError`, `EdaError`, `BrokerError`, `AgentError` per crate |

Not ported because they target Python frameworks: Django, DRF, Channels, Flask / Quart, Litestar,
Celery, pydantic, the pytest plugin, the CloudEvents SDK bridge, OpenTelemetry / structlog /
Sentry observers, and the LLM provider adapters and framework bridges of the agents package.

## Semantic differences

### Engine semantics: both engines follow XState v5

ADR-0001: the headline promise is that a chart behaves as it does in the Stately editor's
simulator, which is the real XState v5 engine. The Python library's engine converged on the same
semantics on its `xstate-v5-engine-parity` branch (unreleased after 0.12.0): the eight
divergences that the conformance suite had recorded -- exit order, parallel entry order,
wildcards matching engine events, sibling-region targets, exit actions on completion, `reenter`
into a descendant, multi-target transitions into parallel regions, and the JSON-form `choose` --
are fixed there. So are ten further engine differences, each now pinned by a conformance case
(`038`–`050`): transitions selected for one event form one microstep with conflict preemption
(grouped exit, then actions, then entry), `done.state.*` goes to the internal queue, guards are
evaluated in XState's selection order (cases may record guard evaluations with
`logic.record_guards`), one `assign` computes every property against the context before it, a
compound self-target without `reenter` resets its children, a JSON `assign` without the
`assignment` wrapper is applied, `always` transitions are selected in the order their states were
entered (not document order), history is recorded only when the state that owns it is exited, an
eventless microstep that changes nothing (a self-targeting `always`) is not re-selected, and
`done.state.*` is raised for the compound parent and every completed parallel ancestor (never a
compound grandparent). `conformance/divergences.generated.md` is empty, and the Python
repository replays the same 50 cases in `tests/test_xstate_conformance_cases.py`. Against
released Python 0.12.0 the old table still applies; it is kept in that release's changelog.

Differences that remain, by design or because no conformance case pins them yet:

| Behaviour | Python (parity branch) | Rust (XState v5) |
|---|---|---|
| Event names | v4 (`done.invoke.*`, `error.platform.*`) | v5 (`xstate.done.actor.*`, `xstate.error.actor.*`) in memory; v4 spellings are accepted on input |
| `invoke` without `input` | `{}` (0.12.0 briefly forwarded the triggering payload; reverted, `event_payload` is the explicit opt-in) | `null`, as XState passes `undefined`; a declared `input` factory sees the event in both |
| Multi-target transitions in diagrams, graph and coverage | only the first target is shown | every target |

These rows describe the engines' designs (ADR-0001, ADR-0002) and are not pinned by a
cross-oracle case.

Snapshots and the structure hash are exempt: they use the Python (v4) event spellings, because
those are the bytes the Python library reads and hashes (ADR-0003). The engine normalises them on
restore. The structure hash renders only the first target of a multi-target transition in both
runtimes, so hashes stay identical.

### Model and API

- **Context and events (ADR-0002).** Two tiers: the dynamic tier (`serde_json::Value` context,
  `Event { ty, payload }`) is what JSON machines, the CLI and persistence use; the typed tier
  makes the engine generic over your context and event types. Actions receive `&mut` context,
  guards `&` context. `actionErrorPolicy: "rollback"` needs a cloneable context.
- **No callables in configuration.** Property-level `assign` functions, `input` / `output`
  factories, computed delays and callable `sendTo` targets exist only as *named, registered
  logic*, never as config values. JSON stays pure data, as in XState v5.
- **Implementations are checked at `start()`.** A missing guard / action / service / delay is an
  `ImplementationMissing` error naming it, before the first event.
- **One algorithm, two drivers (ADR-0004).** The step engine is sans-I/O; `SyncInterpreter` and
  the tokio `Interpreter` execute its effects. There is no `send_threadsafe` or
  `WrongThreadError`: the tokio handle is `Clone + Send`, and thread misuse is a compile error.
- **Defaults.** `actionErrorPolicy` is `fail` (XState); Python's default was `continue`. Set it on
  the chart root to keep the old behaviour. Unknown event types are ignored unless `strict` is set.
- **Restoring resumes timers by default (M7).** `RestoreOptions::default()` uses
  `RestartTimers::Resume`: the remaining time of every `after` timer in the snapshot is honoured
  against the wall clock. Python's bare `SyncInterpreter.from_snapshot(...)` parks them (nothing
  fires until you ask). `persisted()` resumes timers in both libraries, so code that goes through
  `persisted` behaves the same; pass `RestartTimers::Parked` to `from_snapshot` to get Python's
  bare behaviour.
- **Async `run_once` handles a batch, then acks it (M8).** Python awaits the handler of each
  delivery on the event loop and acks it as it goes. `InboundDispatcher::run_once` (async broker)
  pulls a batch, runs the synchronous handlers of up to `max_in_flight` subjects on scoped worker
  threads, and only then acks or requeues every delivery of the batch. A crash in the middle of a
  batch therefore redelivers the whole batch (the inbox absorbs the duplicates), and the calling
  task blocks while handlers run; see [eda.md](eda.md#consuming).
- **Stores are synchronous.** `StateStore` and the log / inbox traits are blocking; from async
  code call them inside `spawn_blocking`. Python's async stores have no separate Rust trait.
- **Plugins become observers.** There is no entry-point discovery. An `Observer` receives
  start / event / dropped / error / stop hooks; attach it with `observe` or through
  `PersistedOptions::observers`. Because observers cannot reach the `persisted` session, the
  outbox plugin buffers envelopes and you flush them inside the closure (see
  [eda.md](eda.md)).
- **Errors are enums.** Receipts carry an `ErrorKind` instead of an exception class name; the
  HTTP status mapping is the same.
- **CLI subset.** `validate`, `inspect` (`--live`), `diagram`, `docs`, `simulate`, `paths`,
  `snapshots`, `info`, `replay` exist; `coverage`, `asyncapi`, `dlq`, `generate-template`,
  `list-templates`, `new`, `plugins`, `update` and `setup` do not yet. `xsm paths --simple` lists
  acyclic paths. `xsm snapshots` reads SQLite and file stores only (`sqlite:///` and `file:///`
  URLs); a `memory://` store is a new empty store per process, so it is refused.
- **`--stale` is a label comparison.** Like Python, `xsm snapshots --stale` flags records whose
  `machine_version` label differs from the `version` of the machine JSON (a machine without a
  `version`, or an unlabelled record, is never flagged). Unlike Python it skips records whose
  snapshot names another machine id. `--stale-by hash` flags a different structure hash instead
  (what restore would refuse), `--stale-by either` flags both, and the table and `--json` always
  carry a `hash_mismatch` flag next to the label comparison.

## What carries over unchanged

Snapshots (layout v4, same fields, order and values; Rust writes compact JSON where Python's
`get_snapshot()` indents), the machine structure hash, SQLite, file, Redis and PostgreSQL layouts,
CloudEvents envelopes, the broker wire formats, the Stately Inspector protocol and `.jsonl`
recordings, saga and circuit-breaker chart JSON, AsyncAPI documents, Mermaid and PlantUML output
and the `Idempotency-Key` / problem-details HTTP behaviour. Stores and brokers are tested in both
directions against the real Python library; the other formats are pinned by goldens generated by
Python and compared with Rust's output. See the interoperability section of the
[README](../../README.md#interoperability-with-the-python-library).

## A migration order that works

1. Run `xsm validate` on your existing chart JSON (export it from Python with `to_json()`); fix
   what the XState-faithful engine reports.
2. Port the logic registry to `MachineLogic` and run the same event scripts with `xsm simulate`
   and your tests, expecting the differences listed above.
3. Point a Rust worker at the existing SQLite / Redis / PostgreSQL store and broker topics next to
   the Python workers; `xsm snapshots --stale` shows records labelled with another machine
   `version` (add `--stale-by hash` to find records whose structure no longer matches).
4. Retire the Python workers once the semantics differences are settled.
