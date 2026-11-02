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
| `RetryPolicy`, `CircuitBreaker`, `SagaBuilder`, `ChoreographyRouter` | the same names (`patterns`) |
| `contrib.starlette`, `contrib.fastapi` | `xstate-axum`: `StatechartRegistry`, `router`, `inspector_router` |
| `contrib.agents` | `xstate-agents` (`agents`) |
| `testing_utils.stub_logic`, `graph` helpers, coverage | `xstate_core::graph` (`stub_logic`, `reachable_states`, `shortest_paths`, `simple_paths`, coverage targets) |
| `xsm` CLI | `xsm` (a subset of the commands, see the README matrix) |
| exceptions (`ImplementationMissingError`, `InvalidConfigError`, ...) | `xstate_core::Error` and its `ErrorKind`; `StoreError`, `EdaError`, `BrokerError`, `AgentError` per crate |

Not ported because they target Python frameworks: Django, DRF, Channels, Flask / Quart, Litestar,
Celery, pydantic, the pytest plugin, the CloudEvents SDK bridge, OpenTelemetry / structlog /
Sentry observers, and the LLM provider adapters and framework bridges of the agents package.

## Semantic differences

### Engine semantics: the Rust engine follows XState v5

ADR-0001: the headline promise is that a chart behaves as it does in the Stately editor's
simulator, which is the real XState v5 engine. The Python engine deviates from XState in a few
places; the Rust engine does not. A fleet that mixes the two shares *state*, not semantics, so
some internal events may be ordered differently until the Python library converges.

| Behaviour | Python | Rust (XState v5) |
|---|---|---|
| `done.state.*` | queued on the external queue | internal queue |
| Transitions selected in one microstep | run one after another, no conflict preemption | optimal enabled set with preemption; grouped exit, then actions, then entry |
| Exit order | ties broken by the state-id string | reverse document order, descendants first, region by region |
| Guard evaluation | every candidate guard is evaluated (memoised) | stops once a transition is selected for a leaf |
| `assign` | applied in place; later property functions see earlier writes | computed against the context before the action list |
| `reenter` | honoured for self-targets only | controls whether a descendant target exits the source |
| Parallel entry order | the targeted region is entered last | document order |
| Event names | v4 (`done.invoke.*`, `error.platform.*`) | v5 (`xstate.done.actor.*`, `xstate.error.actor.*`) in memory; v4 spellings are accepted on input |

Every row is pinned by a conformance case. The cases where the two oracles disagree are generated
into [`conformance/divergences.generated.md`](../../conformance/divergences.generated.md); today
they are:

| Case | Python | XState / Rust |
|---|---|---|
| `exit_order_reverse_document` | exits ordered by depth then id string, regions interleave | reverse document order |
| `parallel_entry_order` | regions off the entry path first, the target's region last | document order |
| `wildcard_vs_engine_events` | `*` and `foo.*` never match engine events: a service failure with only a `*` handler puts the machine in `error` | `*` matches every event, including `xstate.error.actor.*`: the handler consumes the failure |
| `parallel_cross_region_target` | only the target's region is exited and re-entered | the domain is the parallel state: both regions exit and re-enter |
| `exit_actions_on_completion` | reaching a top-level final state runs no further exit actions | exit actions of every active state run, then the machine is done |
| `reenter_descendant_target` | `reenter` ignored for a descendant target | the source is exited and re-entered |
| `multi_target_parallel_entry` | `target: [a, b]` rejected at build time | allowed when the targets are in different regions of a parallel state |
| `choose_and_state_in` | JSON-form `choose` with `params.branches` runs no branch | the first matching branch runs |

Snapshots and the structure hash are exempt: they use the Python (v4) event spellings, because
those are the bytes the Python library reads and hashes (ADR-0003). The engine normalises them on
restore.

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
  `snapshots`, `info`, `replay` exist; `coverage`, `asyncapi`, `dlq`, `generate-template`, `new`,
  `plugins`, `update` and `setup` do not yet.

## What carries over unchanged

Snapshots (layout v4), the machine structure hash, SQLite, file, Redis and PostgreSQL layouts,
CloudEvents envelopes, the broker wire formats, the Stately Inspector protocol and `.jsonl`
recordings, saga and circuit-breaker chart JSON, AsyncAPI documents, Mermaid and PlantUML output
and the `Idempotency-Key` / problem-details HTTP behaviour. See the interoperability section of the
[README](../../README.md#interoperability-with-the-python-library).

## A migration order that works

1. Run `xsm validate` on your existing chart JSON (export it from Python with `to_json()`); fix
   what the XState-faithful engine reports.
2. Port the logic registry to `MachineLogic` and run the same event scripts with `xsm simulate`
   and your tests, expecting the differences listed above.
3. Point a Rust worker at the existing SQLite / Redis / PostgreSQL store and broker topics next to
   the Python workers; `xsm snapshots --stale` shows records written by another machine version.
4. Retire the Python workers once the semantics differences are settled.
