# Feasibility study: porting `xstate-statemachine` (Python) to Rust

**Date:** 2026-11-01
**Subject:** `xstate-statemachine` 0.12.0 (Python, [basiltt/xstate-statemachine](https://github.com/basiltt/xstate-statemachine)) → a Rust library with feature parity and a much faster engine
**Method:** full read of the Python source tree (276 modules, ~3.4 MB of source, 6,162 tests across 472 files), its benchmarks and guides; survey of the Rust crate ecosystem (statechart crates, async, persistence, brokers, observability, FFI).

---

## 1. Verdict

**Feasible, and the niche is open.** No Rust crate today offers an XState-v5-compatible *runtime* with hierarchy, parallel regions, history, `after` timers, `invoke`, actors, snapshots and persistence. The closest crates are either compile-time macro FSMs (`statig`, `finny`, `rust-fsm`, `smlang`) that cannot load a machine from JSON at run time, or the `scxml` crate (0.2.3, released days ago), which is explicitly *"not a runtime executor"* and whose simulator covers "sequential workflows only: single active state, no parallel, no history, no eventless stabilisation". A Rust engine that runs XState JSON unchanged, shares snapshots with the Python library, and is 50–200× faster per event would be the first of its kind.

**Three qualifications:**

1. **"All features" has to be read per layer.** The core engine, persistence, EDA, observability, inspector, testing helpers and CLI are language-neutral and port cleanly (with redesign where Python relies on dynamic typing). The *framework integrations* (FastAPI, Flask, Django, DRF, Channels, Litestar, Starlette, Celery, SQLAlchemy, pytest plugin) have no Rust counterparts; they must be **re-targeted** to the Rust ecosystem (axum/actix, sqlx/sea-orm/diesel, tokio tasks, `cargo test`), not ported.
2. **The Pythonic DSL cannot be copied.** Metaclass scanning, naming-convention hooks and argument injection by parameter name are Python reflection. Rust gets a builder API plus a procedural-macro DSL and a typed `Logic` trait instead — equally ergonomic, but different.
3. **Several Python engine behaviours silently diverge from SCXML/XState v5** (see §4.2). The port must decide, before writing the engine, whether to replicate them for snapshot/parity compatibility or to fix them (and feed the fixes back). This is the single most important early decision.

Rough size: the Python core is ~1 MB of source; a Rust port of core + persistence + EDA + CLI is estimated at 40–60k lines including tests. This is a multi-month, phased project (see §9).

---

## 2. What the Python library actually is (scope inventory)

| Area | Size (source) | Contents |
|---|---|---|
| Core engine | 37 modules, ~1.0 MB | JSON parser/validator, SCXML-style transition algorithm, two engines (sync + asyncio) sharing one algorithm, actions/guards/delays DSL (all XState v5 built-ins), `invoke`/actors/`systemId`, snapshots (layout v4), pure reducer API, clocks, plugins, graph/paths/coverage, Pythonic DSL (class, builder, functional), Mermaid/PlantUML export |
| Persistence | 14 modules, 320 KB | `StateStore` protocol; Memory/File/SQLite stores (+ Redis, SQLAlchemy, Django in contrib); optimistic versioning, pessimistic locks, `persisted()` transaction wrapper; durable `after` deadlines + `DueTimerScanner`; idempotency inbox + receipts; transition/audit log + replay; snapshot migrations; `adopt` of existing rows |
| EDA | 8 modules, 127 KB | CloudEvents envelope, broker protocol + fake broker, inbound dispatcher with dedup/retry, outbox + relay, dead-letter store/plugin, AsyncAPI 3.0 generation |
| Patterns | 6 modules, 76 KB | Retry policy with jitter, circuit breaker (itself a statechart), saga builder, choreography router |
| Inspect | 6 modules, 48 KB | Stately Inspector wire protocol (`@xstate.actor/.event/.snapshot`, pinned to `@statelyai/inspect` 0.7.2), SSE + WebSocket sinks, JSONL recording, replay |
| CLI `xsm` | 64 modules, 700 KB | 20 subcommands (validate, inspect, simulate, paths, coverage, diagram, docs, asyncapi, snapshots, dlq, replay, generate-template with 5 primary + 5 companion templates, new, plugins, info, update, setup), interactive launcher, GitHub Action, pre-commit hooks |
| Contrib | 143 modules, 1.27 MB | Pydantic, Starlette registry, FastAPI, Litestar, Flask/Quart, Django (field, mixin, store, signals, admin, commands, django-fsm migration), DRF, Channels, SQLAlchemy, Redis, Celery, 5 brokers (Redis Streams, Kafka, RabbitMQ, NATS, SQS), CloudEvents SDK, observability (OTel, Prometheus, structlog/loguru/sentry), pytest plugin (fixtures, Hypothesis model tests, coverage gate), **agents** (tool-loop statechart, budgets, allow-lists, structured output, FakeModel, OpenAI/Anthropic providers, trace, LangGraph/pydantic-ai interop) |
| Tests | 472 files, 6,162 tests | includes `test_xstate_v5_parity.py` (5,198 lines), `test_scxml_correctness.py`, `test_engine_conformance.py` — a ready-made conformance oracle for the port |
| Schemas | — | `schemas/xstate-machine.schema.json` (JSON Schema of the accepted config) |

Core invariants worth carrying over verbatim: zero runtime dependencies in core, 90% coverage gate, strict typing (`mypy --strict`), every README code block executed in CI, SemVer with a documented deprecation policy, a security model ("machine definitions are trusted; events and snapshots are not", 1 MiB caps, shape + hash checks on restore).

---

## 3. Rust ecosystem survey

### 3.1 Existing statechart / FSM crates (none is a fit as a base)

| Crate | What it is | Why it is not the base |
|---|---|---|
| `statig` | Hierarchical state machines via `#[state_machine]` macros, `no_std`, state-local storage | Compile-time only; no parallel regions, history, timers, invoke, JSON, snapshots |
| `finny` | Builder + proc-macro HSM with regions, timers, event queue, run-to-completion | Compile-time dispatcher; no data-driven loading, no XState semantics |
| `rust-fsm`, `smlang`, `sm` | Flat FSM DSLs | Flat machines only |
| `scxml` (GnomesOfZurich, 0.2.3, 2026) | W3C SCXML document model: parse, validate (structural + liveness), export DOT/Mermaid/XML/JSON, diff, `ResolvedChart`, rkyv zero-copy, **XState v5 JSON import/export feature**, `parse_untrusted()` with size/depth/identifier limits, WASM | Deliberately *not* a runtime; simulator is single-active-state only; guards never executed; no invoke/timers/actors. **Useful as prior art** for the parser/validator/exporters and its security model, and potentially as an interchange dependency (SCXML XML ⇄ our model) |
| `rustate` 0.3 | XState-inspired, hierarchical + parallel + guards + context | Early (deep history "partially implemented", macros unstable), small community |
| `statechart` (cmars) | 2017, futures 0.1 | Abandoned |
| `state-machines` | Ruby `state_machines` port; typestate + dynamic modes, `after`/`invoke`/`defer`, `Parallel` without cross-region arbitration, `no_std` runner | Different model (event-centric, Ruby semantics); interesting ideas on typestate ⇄ dynamic conversion |

Conclusion: build a new engine; borrow design ideas (and possibly the SCXML XML interchange) from `scxml`, and the macro-ergonomics ideas from `statig`.

### 3.2 Building blocks that are mature and fit

| Need | Crate(s) | Notes |
|---|---|---|
| Serialization | `serde`, `serde_json` (optionally `sonic-rs`/`simd-json` behind a feature for 2–3× faster JSON) | `serde_json` is the compatibility baseline; keep SIMD parsers optional |
| Async runtime | `tokio` | `tokio::time::pause()`/`advance()` (feature `test-util`) covers SimulatedClock-style tests for the async engine; our own `Clock` trait still needed for the sync engine and wall-clock deadlines |
| Logging / tracing | `tracing` + `tracing-subscriber`; `log` compatibility via `tracing-log` | Spans per event / service; near-zero cost when disabled via static max-level features |
| Metrics / OTel | `metrics` facade + `metrics-exporter-prometheus`; `opentelemetry` 0.3x (metrics & logs stable, traces beta) via `tracing-opentelemetry` | Same signal set as the Python `PrometheusPlugin`/`OpenTelemetryPlugin` |
| Errors | `thiserror` (library), `anyhow`/`miette` (CLI) | Explicit error enums with kinds (replaces the Python `str(exception class)` receipt mapping) |
| Data structures | `smallvec`, `fixedbitset` (SIMD set ops), `indexmap`, `slotmap`/`Vec` arenas, string interning (`lasso` or own `u32` ids), `smol_str`/`Arc<str>` | For the compiled machine and active configuration |
| Persistence | `rusqlite` (bundled SQLite), `redis` 1.x (tokio), `sqlx` (Postgres/MySQL/SQLite, async), `sea-orm`/`diesel` optional, `redb`/`sled` embedded KV, `fd-lock`/`fs4` for advisory file locks | All actively maintained; Redis crate is 1.7, `sqlx` 0.8 |
| Brokers | `rdkafka` (librdkafka, dominant), `lapin` (AMQP), `async-nats` (JetStream), `aws-sdk-sqs`, Redis Streams via `redis` | Mirrors the five Python broker adapters |
| Web / inspector | `axum` (+ `tower`), `tokio-tungstenite`/axum WS, SSE via `axum::response::Sse` | Replaces Starlette/FastAPI registry; one HTTP integration crate instead of six |
| Actors (optional) | `kameo`, `ractor` | Likely unnecessary: the XState actor model is small and tightly coupled to the interpreter; implement in-house on tokio tasks + channels |
| CLI | `clap` (derive), `ratatui`/`crossterm` for the interactive launcher, `owo-colors`, `cargo-dist` for binaries | |
| Codegen | `quote`/`prettyplease` for Rust codegen; `schemars` for JSON Schema from types | |
| Python bindings | `pyo3` + `maturin` | Strategic option (§7.6) |
| WASM | `wasm-bindgen` | Core must stay free of OS dependencies for this |
| Testing | `proptest` (+ `proptest-state-machine` for model-based tests), `insta` (snapshot tests), `criterion` + `iai-callgrind` (benchmarks), `cargo-llvm-cov` (coverage), `cargo-fuzz` (parser/snapshot fuzzing), `loom` (if any lock-free code), `miri` in CI | |
| Supply chain | `cargo-deny`, `cargo-audit`, `cargo-semver-checks`, `cargo-msrv` | |

Installed locally: Rust 1.99.1 stable (MSVC), cargo 1.99.1. No extra cargo tools yet.

---

## 4. Feature-by-feature feasibility

Legend: **Direct** = port as is; **Redesign** = same capability, different mechanism; **Re-target** = different ecosystem equivalent; **Defer/Drop** = not meaningful in Rust.

### 4.1 Core engine

| Feature (Python) | Rust | Notes |
|---|---|---|
| XState JSON parser: all keys in `validation.py` whitelists, `x-` extensions, unknown-key policy (`strictConfig`) | Direct | `serde` + `#[serde(deny_unknown_fields)]` is too blunt (needs warn-vs-error and `x-*` passthrough); use a custom `Deserialize` or a two-stage `Value` → typed compile. Reuse `schemas/xstate-machine.schema.json` as the shared contract |
| Shorthands: `"E": "target"`, lists (first match), `null` forbidden transitions, `always`/`on: {"": …}`, `cond`/`internal` v4 aliases | Direct | |
| State types, inferred `initial`, custom `#id`s, dotted-path ids, aliased-cycle detection | Direct | Arena of `StateNode` with `StateId(u32)`, parent/children/depth precomputed |
| Target resolution (`#id.path`, `.child`, bubbling, `strictTargets`) | Direct | Resolve at compile time to `StateId`; keep deprecated sibling fallback behind a flag or drop it (decision) |
| Guards: named, `{type, params}`, `and/or/not`, `"!name"`, `stateIn` | Direct | Guard tree compiled to an enum; built-ins overridable by user names (keep) |
| Actions: all v5 built-ins (`raise`, `sendTo`, `sendParent`, `forwardTo`, `escalate`, `log`, `cancel`, `stopChild`, `spawnChild`, `emit`, `assign`, `pure`, `choose`, `enqueueActions`), camel/snake/`xstate.` spellings, param spec validation | Direct | `enqueueActions` and `pure` need a callback-with-enqueue API; straightforward with closures |
| Callables inside config (`assign` property fns, `input`/`output`/`context` factories, delay fns, callable `sendTo` targets) | Redesign | In Rust these are registered closures referenced by name, or typed closures in a builder. JSON stays pure data (which is also XState v5's model) |
| Event descriptors: exact, `*`, `foo.*`; wildcards never match engine events | Direct | Interned event ids; precomputed partial-wildcard index per state |
| Transition algorithm: selection order, per-leaf first-match, forbidden blocking, domain/LCA, exit/entry ordering, history record/restore, `onDone` propagation, final/output, `reenter` | Direct, with decision | See §4.2 for the divergences to decide on |
| Eventless settling with `maxIterations`, sticky `RunawayChainError`, `chain_trips` | Direct | |
| `actionErrorPolicy` (`continue`/`rollback`/`fail`), `guardErrorPolicy`, `onUnhandled` (`ignore`/`defer`/`error`), deferred-events buffer | Direct | Rollback needs a cheap context snapshot: require `C: Clone` (cheap with persistent/`im` structures or `Arc`), replacing `copy.deepcopy` |
| `after` timers (numeric or named delays, non-finite handling), durable `Deadline` records | Direct | |
| `invoke`: child machines, sync fns, async fns, long-lived actor logic (`from_callback`, `from_iterator`, …), SCXML §6.1 deferral, ignore-results-after-exit | Redesign | Python's "any callable" becomes an `ActorLogic` trait with variants (`FromFuture`, `FromCallback`, `FromMachine`, `FromStream`). Sync engine supports only sync logic (as Python does) |
| Actor registry, `systemId`, `sendTo` resolution order, `sendParent`, `emit` listeners | Direct | |
| Two engines (sync + async) sharing one algorithm | Redesign (better) | Python runs one `async def` algorithm synchronously via `coro.send(None)`. In Rust use a **sans-I/O core**: a pure `step(&Machine, &mut State, Event) -> Effects` function; `SyncInterpreter` and tokio `Interpreter` are thin drivers that execute `Effects` (arm timer, start service, deliver event). One algorithm, zero duplication, trivially testable |
| Bounded inbox, overflow policies, `send_threadsafe`, `WrongThreadError`, `ReentrantWaitError` | Redesign | Ownership + `Send`/`Sync` bounds make most thread misuse a compile error. Keep overflow policies on the async mailbox (`tokio::sync::mpsc` bounded) |
| Receipts (`state_ids, changed, error, deferred, denied, duplicate`) and HTTP status mapping | Direct | Replace exception-class-name mapping with an `ErrorKind` enum |
| Snapshots: layout v4, `machine_hash`, `check_shape`, `upcast`, `from_snapshot` policies, `from_state`, restore semantics (no entry actions, opt-in `restart_services`/`restart_timers`) | Direct | **Recommend byte-compatible snapshots** so Python and Rust can share a store. Requires reproducing the structure hash exactly (sorted-key canonical JSON → SHA-256 prefix) — doable, must be tested against fixtures generated from Python |
| Pure API (`initial_transition`, `transition`, `PureSnapshot`, `wait_for`, `to_promise`) | Direct | Falls out of the sans-I/O core for free |
| Clocks (`RealClock`, `SimulatedClock`, `wall_now`) | Direct | `Clock` trait; `SimulatedClock` with `increment/set` |
| Plugins (`PluginBase` with ~30 hooks, `_SafePlugin` isolation, global registry, `LoggingInspector`, redaction) | Redesign | `trait Plugin: Send + Sync` with default no-op methods; isolation via `catch_unwind` is possible but discourage panicking plugins; hook errors returned as `Result`. `LoggingInspector` becomes `tracing` events |
| Plugin discovery via entry points | Drop | No runtime plugin discovery in Rust; features + explicit registration. (Dynamic loading via `libloading`/WASM plugins is possible but out of scope) |
| Pythonic DSL (`StateMachine` metaclass, `State`, `|` groups, naming-convention hooks, `@action/@guard/@service`, argument injection) | Redesign | (a) fluent `MachineBuilder` (direct), (b) `build_machine()` functional form (direct), (c) `#[derive(StateMachine)]`/`statechart!{}` proc-macro DSL producing typed `State`/`Event` enums and a `Logic` trait with one method per named action/guard/service. Injection-by-name is replaced by typed signatures (`fn(&mut Ctx, &Event) -> …`) |
| `to_json`, `to_mermaid`, `to_plantuml` | Direct | |
| Graph utilities (`shortest_paths`, `simple_paths`, `reachable_states`, `transition_coverage_targets`), `CoverageCollector`, `stub_logic` | Direct | Run the pure step function; will be orders of magnitude faster than Python's 54 s for 3,456 configurations |
| Validation (`RootTargetError`, dead `always` loops, strict `raise` targets, unresolvable targets, logic-name matching, `ImplementationMissingError`, `context_validator`) | Direct | Compile-time (machine build) checks; `ImplementationMissingError` surfaces at `build()` because the logic registry is known then — strictly better than Python's run-time discovery |
| Exception taxonomy (~30 classes) | Direct | One `Error` enum (+ sub-enums for Store/Snapshot) with `thiserror`; keep names for documentation parity |

### 4.2 Engine semantics to decide before coding

The inventory found undocumented divergences from SCXML / XState v5 in the Python engine. Each must be a conscious choice in the Rust port because parity tests and shared snapshots depend on it:

| Python behaviour | SCXML / XState v5 | Recommendation |
|---|---|---|
| `done.state.*` goes on the **external** queue | Internal queue | Fix in Rust (internal); mirror fix back to Python — observable ordering difference only when external events are already queued |
| Multiple transitions selected in one step run sequentially with **no conflict preemption**; only skipped if source became inactive | Optimal enabled set with preemption, grouped exit/entry | Implement SCXML preemption; verify with `test_scxml_correctness.py` cases; expect a handful of Python tests to encode the old behaviour |
| Exit-order ties broken by **id string**, not reverse document order | Reverse document order | Fix (document order is free once nodes are in an arena) |
| **Every** candidate guard is evaluated (memoised), no short-circuit | Only until a transition is chosen | Fix (short-circuit); plugin `on_guard_evaluated` counts will change |
| `assign` applied in place, property fns see earlier keys' writes | v5 applies the whole assignment against the pre-action context | Decide; XState v5 semantics are cleaner for typed contexts |
| `reenter` ignored for non-self targets | Affects whether source is exited when target is a descendant | Follow v5 |
| Parallel: targeted region entered **last** | Document order | Follow document order |
| v4-style event names (`done.invoke.*`, `error.platform.*`), private init/exit event names | v5 uses `xstate.done.actor.*`, `xstate.error.actor.*`, `xstate.init` | Accept both on input; emit the v5 spelling with a compatibility alias — but this interacts with snapshot/log compatibility; decide together with §4.1 snapshot policy |

Everything else (deferred invokes, ignoring results from exited states, guard-error fall-through, first-`onDone`-ancestor rule, machine `output` precedence, `maxIterations`) matches the specs and should be kept.

### 4.3 Persistence

| Feature | Rust | Notes |
|---|---|---|
| `StateStore` contract (`load/save/delete/forget/list_keys/lock/health`, `expected_version` with `0 = must not exist`, 200-char keys, 1 MiB cap, codec seam) | Direct | Sync trait + async trait (`async fn` in traits is stable); `as_async` adapter via `spawn_blocking` |
| `MemoryStore`, `FileStore` (percent-encoded names, temp + rename, advisory lock, stale-lock reclaim, 0600 perms), `SQLiteStore` (WAL, `BEGIN IMMEDIATE`, schema v2) | Direct | `fd-lock`/`fs4`, `rusqlite` bundled. Windows share-mode semantics need care (Python already handles `msvcrt.locking`) |
| Redis store (Lua atomic save, `SET NX PX` fencing, zset deadlines), SQLAlchemy/Django stores | Re-target | Redis: `redis` crate with the same Lua scripts. SQL: one `sqlx` store covering Postgres/MySQL/SQLite with the same table layout (`xsm_snapshots`, `xsm_deadlines`, `xsm_locks`, `xsm_inbox`, `xsm_transitions`, `xsm_outbox`) so Python and Rust workers can share a database |
| Lock strategies (`OptimisticLock` with jittered backoff, `PessimisticLock`, `NoLock`), `persisted()`/`apersisted()`, `after_commit`, mark buffering with `flush_priority`, failure semantics (§7 of guarantees) | Direct | `persisted` becomes a closure-taking function or a guard object whose `Drop`/explicit `commit()` saves; recommend explicit `commit()` (Drop cannot fail) |
| Durable deadlines (`Deadline` records, `restart_timers` = `restart`/`resume`/`fire_due`, entry_seq, newest-wins), `DueTimerScanner` (`due_keys`, `_Held` fence, skew tolerance, `ScanResult`) | Direct | |
| Idempotency inbox (`claim/mark/release`, scope escaping, fingerprint, in-snapshot ring of 64, fail-closed policies), `IdempotencyPlugin` | Direct | |
| Transition log / audit (`TransitionRecord`, Memory/JSONL/SQLite logs, `replay` with divergence detection) | Direct | |
| `SnapshotMigrator`, `MachineVersionMismatchError` policies | Direct | |
| `adopt.from_state_ids` | Direct | |

### 4.4 EDA and patterns

All language-neutral: **Direct** port. `Envelope` (CloudEvents 1.0, UUIDv7 ids, extension validation, traceparent), broker traits + fake broker, `InboundDispatcher` (batching, requeue order, attempt counting, dead-lettering reasons), `OutboxStore`/`OutboxRelay` with leases, dead-letter stores + audited replay, AsyncAPI 3.0 document generation, `RetryPolicy`, `CircuitBreaker` (as a statechart), `SagaBuilder` (generates XState JSON — identical output), `ChoreographyRouter`. Broker adapters re-target to `rdkafka`, `lapin`, `async-nats`, `aws-sdk-sqs`, Redis Streams.

### 4.5 Observability and inspector

| Feature | Rust |
|---|---|
| `LoggingInspector`, structlog/loguru/sentry plugins | `tracing` events/spans in core; `sentry` crate integration optional |
| `OpenTelemetryPlugin` (span per event, guard events, child spans per service), `PrometheusPlugin` metric set, `LabelGuard` hygiene | `tracing-opentelemetry` + `metrics` facade; same metric names and label policy |
| Stately Inspector protocol (3 message types, session ids, deny-by-default context, redaction), SSE sink with token/Host/Origin/CSP hardening, WebSocket sink, JSONL recording + `replay` with speed | Direct; `axum` SSE/WS. Protocol fixtures from Python's recorded sessions become shared test vectors |

### 4.6 CLI `xsm`

| Command | Rust |
|---|---|
| `validate`, `inspect` (incl. `--live`), `simulate` (interactive + scripted + `--record`), `paths`, `coverage`, `diagram`, `docs`, `asyncapi`, `snapshots`, `dlq`, `replay`, `info` | Direct (`clap`, `ratatui` for the TUI). Faster and single-binary |
| `generate-template` (Python code in 5 styles + pytest/typed/plugin/pydantic/fastapi companions, `--check`/`--diff`, structural verification of the output) | Redesign: generate **Rust** (typed `Event`/`State` enums, a `Logic` trait with stubs, `#[test]`s replaying engine-recorded paths, optional axum router). Keep the `--check` drift gate and the "compile the output and diff the machine structure" verification idea |
| `new` (fastapi/flask scaffolds) | Re-target: `axum` scaffold |
| `plugins`, `update`, `setup` (Windows launcher shim) | Drop (`plugins`, `setup`); `update` optional via `self_update` |
| GitHub Action, pre-commit hooks | Direct: composite action that installs the `xsm` binary (`cargo-dist`/`cargo-binstall`), pre-commit `language: rust` or binary hooks |

### 4.7 Framework integrations

| Python | Rust equivalent | Verdict |
|---|---|---|
| Starlette registry / FastAPI router / Litestar / Flask / Quart | One `xstate-axum` crate: `StatechartRouter` (`GET /{key}`, `POST /{key}/send`, per-event routes, SSE `/stream`, WebSocket, RFC 9457 problem+json, `Idempotency-Key`, health/ready, timer-scanner background task) + `tower` layers; optionally `actix-web` later | Re-target |
| Django field/mixin/signals/admin/commands, DRF, Channels | No equivalent framework. SQL store via `sqlx` covers the data side; signals → plugin hooks; admin → none | Re-target / Drop |
| SQLAlchemy mixin (`send_with_retry`, column sync in the same flush) | `sqlx` store + helper; ORM-level mixins possible for `sea-orm` later | Re-target |
| Celery (`celery_service`, `@statechart_task`, Beat scheduler, outbox relay task) | tokio tasks / `apalis` job queue; scanner and relay as long-running tasks | Re-target |
| Pydantic (`EventModel`, typed context, codec, machine config models/schema) | `serde` + `schemars` typed events/context — stronger, compile-time | Redesign |
| pytest plugin (fixtures, markers, `given/when/then`, Hypothesis model tests, coverage report + gate) | `xstate-testing` crate: fixtures as builders, `proptest-state-machine` for model-based testing with shrinking, `CoverageCollector` + `cargo xsm coverage` gate, `insta` snapshot tests | Redesign |
| Observability | see §4.5 | |
| Brokers, CloudEvents | see §4.4 | |
| **Agents** (tool-loop chart, `agent_logic`, tool registry with JSON-schema-from-signature, allow-lists, budgets, structured output with retries, `FakeModel`, OpenAI/Anthropic providers, trace, multi-agent, LangGraph/pydantic-ai bridges) | Feasible as `xstate-agents`: charts are JSON (identical), tool schemas via `schemars` derive, providers via `async-openai`/`anthropic` crates or raw `reqwest`. Drop the Python-framework bridges | Redesign (phase 5) |

---

## 5. Proposed Rust architecture

### 5.1 Workspace layout

```
xstate-statemachine-rs/
  crates/
    xstate-core/        # machine model, parser/validator, sans-I/O step engine, sync interpreter,
                        # snapshots, pure API, clock, plugins trait, graph, exporters. Zero I/O.
                        # features: serde (default), simd-json, wasm
    xstate-macros/      # #[derive(...)] / statechart!{} DSL, typed Event/State enums, Logic trait
    xstate-tokio/       # async Interpreter: mailbox, timers, invoke/actor runtime, SimulatedClock bridge
    xstate-persistence/ # StateStore traits, Memory/File/SQLite, locks, persisted(), deadlines, scanner,
                        # inbox, log, migrator, adopt. features: sqlite, redis, sqlx-postgres, sqlx-mysql
    xstate-eda/         # Envelope, brokers trait, fake broker, dispatcher, outbox, DLQ, AsyncAPI;
                        # features: kafka, rabbitmq, nats, sqs, redis-streams
    xstate-patterns/    # retry, circuit breaker, saga builder, choreography
    xstate-observability/ # tracing layers, metrics, OpenTelemetry, Prometheus
    xstate-inspect/     # Stately Inspector protocol, SSE/WS sinks (axum), JSONL record/replay
    xstate-axum/        # HTTP/WS integration
    xstate-testing/     # fixtures, proptest strategies, model-based tests, coverage gate
    xstate-agents/      # (later) tool-loop agent runtime
    xsm/                # CLI binary (clap + ratatui)
    xstate-py/          # (optional) pyo3 bindings
  conformance/          # JSON fixtures exported from the Python test suites + Python-generated snapshots
  benches/              # criterion + iai-callgrind, budgets.json gate like Python's perf_gate.py
```

The facade crate `xstate-statemachine` re-exports the common set with feature flags (`tokio`, `persistence`, `sqlite`, `redis`, `eda`, `inspect`, `axum`, …), mirroring Python's extras.

### 5.2 Key design decisions

1. **Sans-I/O step engine.** `fn step(machine: &Machine, state: &mut ActiveState<C>, event: &Event, logic: &dyn Logic<C>, out: &mut Effects) -> Result<StepReport>`. Effects are a `SmallVec` of `ArmTimer{state, delay, entry_seq}`, `CancelTimers(state)`, `StartInvoke(..)`, `StopActor(..)`, `Enqueue(event, lane)`, `Emit(..)`, `SendTo(..)`. The sync interpreter executes them inline with a timer heap; the tokio interpreter maps them onto tasks and channels. This yields: one algorithm, the pure API for free, trivially deterministic tests, and WASM compatibility of the core.
2. **Compiled machine representation.** Arena `Vec<StateNode>` indexed by `StateId(u32)`; `parent`, `children: Range`, `depth`, `ancestors` precomputed; transitions in a flat `Vec<Transition>` with per-state `on: HashMap<EventId, SmallVec<[TransitionId; 2]>>` plus wildcard list; interned `EventId(u32)` with an interner that also stores the original string; domain/exit-set/entry-path memoised per transition at build. Active configuration as `FixedBitSet` (fast union/intersection for `stateIn`, final checks) plus an ordered leaf list.
3. **Context typing: two tiers.** `C = serde_json::Value` (`DynContext`) for JSON/Python parity and dynamic use; any `C: Serialize + DeserializeOwned + Clone + Send + Sync + 'static` for typed use (actions receive `&mut C`). `assign` in JSON form operates on `DynContext`; typed machines use closures. Rollback uses `Clone` (cheap `Arc`/persistent structures recommended in docs).
4. **Events.** `Event { ty: EventId, payload: serde_json::Value }` as the dynamic form; typed `E: Serialize + DeserializeOwned` with a derive that maps variant names to event types for the typed tier. Provenance (engine-generated vs user) is a private field, as in Python.
5. **Logic registry.** `MachineLogic<C>` holds `HashMap<Arc<str>, Action<C>>`, guards, services (`ActorLogic`), delays. Names validated at `build()` → `ImplementationMissingError` at compile time of the machine. The `statechart!{}` macro generates a `trait Logic { fn is_unlocked(&self, ctx:&C, ev:&Event)->bool; fn count_open(&mut self, ctx:&mut C, ev:&Event); … }` from the JSON embedded at compile time (`include_str!`), so renaming a guard in Stately becomes a compile error in Rust — a strictly stronger guarantee than Python's codegen `--check`.
6. **Type safety.** Newtypes for `StateId`, `EventId`, `TransitionId`, `ActorId`, `Version`, `EntrySeq`; typestate on the interpreter lifecycle (`Interpreter<Uninitialized>` → `start()` → `Interpreter<Running>`) where it does not hurt ergonomics; `#![forbid(unsafe_code)]`, `#![deny(missing_docs, unreachable_pub)]`, clippy `pedantic` with documented allows; `#[non_exhaustive]` on public enums for SemVer room.
7. **Errors.** `xstate_core::Error` enum via `thiserror` with the Python taxonomy as variants and an `ErrorKind` for receipts/HTTP mapping; `Result<T>` everywhere, no panics on user input (fuzzed).
8. **Logging.** `tracing` only in library crates (`target = "xstate"`), spans `xstate.event`, `xstate.service`, structured fields (`machine.id`, `event.type`, `state.ids`), payload/context redaction at the subscriber boundary (deny-by-default like the Python inspector). No `println!`.
9. **Security.** Config, events, snapshots parsed with limits (size 1 MiB default, depth, state/transition counts — the `scxml` crate's `parse_untrusted` table is a good model); guards/actions are names, never code; snapshot restore validates shape + `machine_hash`; redaction key list shared with Python; `cargo-deny` (licenses, advisories, duplicate deps), `cargo-audit` in CI; fuzz targets for parser, snapshot restore and envelope.
10. **Snapshot and store wire compatibility with Python** (layout v4, SQLite/Redis/SQL table layouts, CloudEvents envelope, inspector protocol, AsyncAPI output, Mermaid output). This makes "Rust worker + Python API" and gradual migration possible, and gives free conformance fixtures.
11. **Concurrency model.** Sync interpreter is `!Sync` by design (single owner; cross-thread sends through a `Mailbox` handle); tokio interpreter owns its state inside a task and exposes `ActorRef`-style handles (`send`, `send_and_wait -> Receipt`, `subscribe`, `snapshot`). `SimulatedClock` works for both; tokio's paused time is an additional option for tests.

---

## 6. Performance: what to expect

Python reference (sync engine, 0.11.0, i7, `benchmarks/competitors/results.json`): flat toggle 71k events/s (≈14 µs), guard+action 62k/s, 3-level nested 28.5k/s (≈35 µs), parallel 25k/s (≈39 µs), construction 8.7k machines/s (≈115 µs), snapshot get 12 µs, restore 23 µs, `persisted()` memory round-trip 87 µs, SQLite 140 µs. Async aggregate ≈59k events/s per process (single loop thread).

Estimated Rust (interpreted, data-driven engine; release build; no `tracing` subscriber attached):

| Path | Python | Rust estimate | Factor |
|---|---|---|---|
| Flat event, no actions | 14 µs | 50–150 ns | ~100× |
| Hierarchical / parallel event | 35–39 µs | 150–500 ns | ~100× |
| Event with guard + `assign` on `DynContext` | 16 µs | 200–600 ns (JSON `Value` mutation dominates); typed context: ~100 ns | 30–150× |
| Build machine from JSON (50 states) | ~115 µs (sync adapter S5) | 10–40 µs (`serde_json` parse dominates; `scxml` crate measures 27.6 µs parse+validate for 50 states) | 3–10× |
| Snapshot get / restore | 12 / 23 µs | 1–3 / 2–5 µs | ~5–10× |
| `persisted()` memory / SQLite | 87 / 140 µs | 3–8 / 30–80 µs (SQLite fsync-bound; same order as Python's SQLite because the disk is the limit) | 10–25× / ~2× |
| Throughput per process | 59k events/s (one loop thread) | millions/s; scales with cores via independent interpreters | 50–100× + multicore |
| Memory per interpreter | several KB of Python objects | hundreds of bytes (bitset + small vecs) + context | ~10× |

The honest caveat: the Rust/Python gap is largest for pure engine work and smallest where I/O dominates (SQLite, brokers, HTTP). A CI perf gate (criterion + `iai-callgrind` instruction counts, budgets file like Python's `perf_gate.py`) should be part of Phase 1 so regressions are caught from the start.

---

## 7. Good-practice checklist for the Rust project

1. **Type safety:** newtype ids, typestate lifecycle, `#[non_exhaustive]`, no `unwrap` in library code (clippy `unwrap_used` denied), generics with sane defaults (`Interpreter<C = DynContext, E = Event>`).
2. **Safety:** `#![forbid(unsafe_code)]` in all crates except a possible SIMD-JSON feature (which is in a dependency anyway).
3. **Errors:** `thiserror` enums, `source()` chains, stable `ErrorKind`, documented per public fn (`# Errors` sections enforced by clippy `missing_errors_doc`).
4. **Logging:** `tracing` with `target`s and `level` filters; feature `log` for `log` crate users; redaction of context/payload by default.
5. **Security:** limits on untrusted input, fuzzing, `cargo-deny`, `cargo-audit`, `SECURITY.md` mirroring the Python trust model; no network or filesystem in core.
6. **Performance:** `criterion` + `iai-callgrind`, budgets file, `#[inline]` discipline on the hot path, `SmallVec`/bitset, interning, `#[cold]` error paths, allocation-free step for the common case; PGO/LTO in release profile.
7. **Testing:** port the three Python conformance suites as JSON fixtures (`conformance/`), `proptest` for the parser and algorithm invariants (configuration legality, exit/entry pairing, history), `insta` for exporters, doctests on every public item, `cargo-llvm-cov` ≥ 90% gate as in Python, `miri` on the core in CI, `loom` if any lock-free code appears.
8. **Docs:** `docs.rs` with `#![doc = include_str!("../README.md")]`, `#![deny(rustdoc::broken_intra_doc_links)]`, an `mdbook` guide mirroring `docs/_guide`, executable examples (`examples/`) run in CI.
9. **Release hygiene:** MSRV policy (e.g. stable − 2, checked with `cargo-msrv`), `cargo-semver-checks` in CI, `cargo-release`, `cargo-dist` for `xsm` binaries (Windows/macOS/Linux), signed releases.
10. **Interoperability:** keep wire formats identical to Python (snapshot v4, store schemas, envelope, inspector protocol) and add a cross-language conformance job that runs Python-produced fixtures against Rust and vice versa.

### 7.6 Strategic option: `pyo3` bindings

Once the Rust core exists, `xstate-py` (pyo3 + maturin) could become the engine behind the Python package (the `pydantic-core` pattern). Expected gains for Python users: machine building, validation, snapshots, graph/paths and the pure API would be 10–100× faster; per-event dispatch is bounded by the cost of calling Python guards/actions (~1 µs per callback crossing), so a 3–10× gain on busy machines rather than 100×. This is worth a separate decision after Phase 1; the sans-I/O core makes it cheap to try.

---

## 8. Risks and open decisions

| # | Risk / decision | Impact | Mitigation |
|---|---|---|---|
| 1 | Semantics: replicate Python's divergences (§4.2) or follow SCXML/XState v5 | Determines which Python tests port unchanged and whether snapshots/logs are interchangeable | Decide in Phase 0; recommend "follow the spec, fix Python in a coordinated minor release"; keep an `engine_compat = "python-0.12"` switch only if a concrete need appears |
| 2 | Scope creep from "all features" (143 contrib modules) | Multi-month slip | Define parity as *capability parity* per §4; Python-framework adapters are explicitly re-targeted |
| 3 | Dynamic context (`serde_json::Value`) performance and ergonomics | Could erode the speed story for JSON-driven users | Two-tier context; benchmark both from day one; consider `simd-json` `OwnedValue` or a custom compact value type later |
| 4 | Async/sync duplication | Doubles bug surface | Sans-I/O core (§5.2) |
| 5 | Snapshot/hash byte compatibility with Python | Subtle mismatches break shared stores | Fixture-driven tests generated from Python; canonical JSON writer with sorted keys and Python-compatible float/str formatting |
| 6 | Windows file locking, SQLite on network shares | Same as Python's known limits | Reuse Python's documented limitations; `fd-lock`; tests on Windows CI |
| 7 | Macro DSL complexity | Proc-macros are costly to maintain | Ship builder API first; macro in Phase 1b after the model stabilises |
| 8 | `rdkafka` requires a C toolchain (librdkafka) | Build friction for users | Feature-gate; offer pure-Rust `krafka`/`rskafka` later if they mature |
| 9 | OpenTelemetry traces are still "beta" in Rust | API churn | Keep OTel behind a feature and pin |
| 10 | Maintainer bandwidth (one author) | — | Phase and publish incrementally; `xstate-core` alone is already a useful 0.1 |

---

## 9. Phased roadmap (estimates, engineering effort)

| Phase | Deliverable | Est. |
|---|---|---|
| 0. Design spike | Semantics decisions (§4.2), context model, snapshot compat policy, workspace skeleton, CI (fmt/clippy/deny/audit/coverage/bench), conformance fixture exporter from the Python tests | 1–2 weeks |
| 1. Core | `xstate-core`: parser/validator, compiled machine, sans-I/O step engine (all state types, history, `always`, `after`, guards/actions DSL, `onDone`/output, policies), sync interpreter, clocks, pure API, snapshots v4 + hash, plugins trait, exporters, graph; `xstate-tokio` interpreter with invoke/actors/`systemId`; builder API; parity suite ≥ 95% of the Python v5-parity + SCXML cases | 8–12 weeks |
| 1b. DSL + testing | `xstate-macros` typed DSL and `Logic` trait; `xstate-testing` (fixtures, proptest model tests, coverage gate) | 3–4 weeks |
| 2. Persistence | Stores (memory/file/sqlite/redis/sqlx), locks, `persisted`, durable deadlines + scanner, inbox + receipts, log + replay, migrator, adopt; cross-language store fixtures | 5–7 weeks |
| 3. Observability, inspector, CLI | `tracing`/metrics/OTel/Prometheus, inspector protocol + sinks + replay, `xsm` with validate/inspect/simulate/paths/coverage/diagram/docs/asyncapi/snapshots/dlq/replay, Rust codegen with `--check`, GitHub Action, pre-commit | 5–7 weeks |
| 4. EDA | envelope, dispatcher, outbox/relay, DLQ, AsyncAPI, patterns (retry/breaker/saga/choreography), five broker adapters | 4–6 weeks |
| 5. Integrations and extras | `xstate-axum`, `xstate-agents`, optional `pyo3` bindings, SCXML XML interchange via the `scxml` crate | ongoing |

Total to capability parity (phases 0–4): roughly **6–9 months of focused effort**, with a publishable, independently useful `xstate-core` + `xstate-tokio` at the end of Phase 1 (~3 months).

---

## 10. Recommendation and immediate next steps

1. Go ahead; start with Phase 0. The project is well-defined because the Python library already has a specification-grade test suite, a JSON schema, documented guarantees and benchmarks to port.
2. Decide the three design questions now: (a) spec-faithful vs Python-faithful engine semantics (recommend spec-faithful + fix Python); (b) two-tier context with `serde_json::Value` default (recommend yes); (c) byte-compatible snapshots and store schemas with Python (recommend yes — it is the differentiator that lets Python and Rust services share state).
3. Write the conformance exporter first: a small Python script that walks `tests/test_xstate_v5_parity.py`, `test_scxml_correctness.py` and `test_engine_conformance.py` style cases and emits JSON fixtures (`machine`, `events`, `expected state_ids/context/actions`), plus Python-generated snapshots and inspector recordings. The Rust engine is "done" when it passes them.
4. Set the performance gate on day one (criterion + instruction counts + `budgets.json`), targeting ≤ 200 ns per flat event and ≤ 500 ns per hierarchical event on a laptop core.

---

## Appendix A. Python public surface the port must cover (checklist)

- Machine: `create_machine(config, logic=, logic_modules=, logic_providers=, input=, strict_config=, strict_targets=, context_validator=, event_schemas=)`, `MachineNode.to_json/to_mermaid/to_plantuml`, `version`, `structure_hash`.
- Interpreters: `SyncInterpreter`, `Interpreter` (`max_queue_size`, `overflow_policy`, `service_pool_size`), `start/stop/send/send_threadsafe/send_events/tick/wait_done/await_settled/subscribe/on/can/matches/has_tag/get_meta/value/tags/queue_depth/last_error/chain_trips/clear_chain_error`, `get_snapshot/get_persisted_snapshot/from_snapshot/from_state`, `use(plugin)`, `strict`, `input`.
- Events: `Event`, `StreamEvent`, `DoneEvent`, `ErrorEvent`, `AfterEvent`, `Receipt`, `receipt_to_json/from_json/to_status`.
- Logic: `MachineLogic(actions, guards, services, delays)`, `LogicLoader`, built-ins (`raise, sendTo, sendParent, forwardTo, escalate, log, cancel, stopChild, spawnChild, emit, assign, pure, choose, enqueueActions`), guards (`and/or/not/stateIn`), actor logic creators (`from_callback, from_iterator, from_async_iterator, from_coroutine, from_callable, from_interpreter`).
- Clocks: `Clock`, `RealClock`, `SimulatedClock`.
- Pure API: `initial_transition, transition, get_initial_snapshot, get_next_snapshot, PureSnapshot, wait_for, wait_for_sync, to_promise`.
- Plugins: `PluginBase` (30 hooks), `LoggingInspector`, `register_global/unregister_global`, `CoverageCollector`.
- Graph: `shortest_paths, simple_paths, reachable_states, transition_coverage_targets, stub_logic, logic_names`.
- Pythonic: `State, StateMachine, MachineBuilder, build_machine, transition, action, guard, service` → builder + macro DSL.
- Exceptions: full taxonomy in §4.1.
- Persistence, EDA, patterns, inspect, CLI, contrib: as itemised in §4.3–4.7.

## Appendix B. Sources

- Python source and docs: `src/xstate_statemachine/**`, `docs/_guide/{guarantees,persistence,production-characteristics,security,cli,integrations}.md`, `docs/FEATURE_GAP_ANALYSIS.md`, `benchmarks/{budgets.json,perf_gate.py,scaling.py,competitors/}`, `schemas/xstate-machine.schema.json`, `tests/test_xstate_v5_parity.py`.
- Rust crates: [statig](https://docs.rs/statig), [finny](https://docs.rs/finny), [scxml](https://crates.io/crates/scxml), [rustate](https://crates.io/crates/rustate), [state-machines](https://docs.rs/state-machines), [cmars/statechart](https://github.com/cmars/statechart), [tokio time testing](https://tokio.rs/tokio/topics/testing), [sonic-rs](https://docs.rs/sonic-rs), [simd-json](https://docs.rs/simd-json), [opentelemetry-rust status](https://github.com/open-telemetry/opentelemetry-rust), [redis-rs](https://crates.io/crates/redis), [fred](https://docs.rs/fred), [rdkafka](https://crates.io/crates/rdkafka), [Kafka Rust clients comparison](https://kafma.app/blog/kafka-rust-clients), [maturin](https://github.com/pyo3/maturin), [Rust actor library comparison](https://tqwewe.com/blog/comparing-rust-actor-libraries), [fixedbitset](https://docs.rs/fixedbitset), [slotmap](https://docs.rs/slotmap).
- Specs: [W3C SCXML](https://www.w3.org/TR/scxml/) (Appendix D algorithm), [Stately inspection events](https://stately.ai/docs/inspection), [@statelyai/inspect](https://github.com/statelyai/inspect), [XState](https://github.com/statelyai/xstate).
