# QA handover: `xstate-statemachine` for Rust

**Audience.** An autonomous QA / review agent (or engineer) taking over this repository to battle-test
it end to end before its first public release and to build the documentation site that will serve as
the library's reference ("the bible").

**Status at handover.** Repository `C:\Users\basil\Desktop\Projects\RustProjects\xstate-statemachine-rs`,
branch `main`, head `d247434` (2026-11-02), clean tree, **no remote yet** (nothing has been pushed to
GitHub; CI has never run). Workspace version `0.1.0`, unpublished. Roughly 75k lines of Rust across 15
crates, 20k of them tests; 1041 cargo tests + 32 pytest tests pass; `cargo fmt --check`, clippy pedantic
with `-D warnings` on all targets/features, rustdoc with `-D warnings` and `cargo deny check` are clean.

---

## 1. Why this exists

The owner (Basil T T, GitHub `basiltt`) maintains a Python library, **`xstate-statemachine`**
(local checkout `C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine`, version
0.12.0; PyPI has 0.11.0; GitHub `basiltt/xstate-statemachine`). It is a feature-rich XState v5 /
SCXML statechart runtime for Python: engine, async interpreter, persistence with durable timers and
idempotency, event-driven-architecture tooling (CloudEvents envelopes, brokers, outbox, dead letters,
AsyncAPI), resilience patterns, observability, the Stately Inspector protocol, a CLI (`xsm`), HTTP
integrations, LLM-agent charts, and Python-framework adapters.

This repository is the **Rust port**. The brief from the owner:

> All supported features of the Python version should also be available for Rust. Rust is meant for
> speed, so we need a library which is super ultra fast and at the same time provides all the
> features of the Python library, with all good practices followed in Rust: full type safety,
> security, high performance, logging.

Two further requirements shaped the design:

1. **Stately editor compatibility.** A machine built in the [Stately visual editor](https://stately.ai/editor)
   and exported as XState v5 JSON must run unchanged. The engine is therefore specified against the
   *real* XState v5 engine (Node), not against the Python library, wherever the two differ.
2. **Interoperability with the Python library.** Snapshots, structure hashes, SQLite / Redis /
   PostgreSQL layouts, CloudEvents envelopes, inspector recordings, saga and circuit-breaker charts and
   AsyncAPI documents are byte- or value-compatible so that Python and Rust workers can share stores,
   brokers and tooling, and a Python user can switch to the Rust engine through the PyO3 bindings.

The feasibility study that started the project is `docs/feasibility-report.md`; the design
decisions are `docs/adr/0001`–`0005`.

## 2. What it is (architecture in one page)

```
xstate-statemachine (facade crate, feature flags = Python extras)
├── xstate-core          machine model, JSON parser/validator, compiler, sans-I/O step engine
│                        (W3C SCXML Appendix D as realised by XState v5), SyncInterpreter,
│                        snapshots (layout v4) + structure hash, pure API, builder, exporters
│                        (Mermaid/PlantUML/JSON), graph utilities, observers, redaction, clocks,
│                        typed traits (TypedEvent/TypedState)
├── xstate-tokio         async actor runtime: mailbox, timers, invoked services, child machines,
│                        sendParent/sendTo, System registry
├── xstate-persistence   StateStore contract; Memory/File/SQLite stores (+ Redis, PostgreSQL behind
│                        features); optimistic/pessimistic locks; persisted() transaction wrapper;
│                        durable `after` deadlines + DueTimerScanner; idempotency inbox + receipts;
│                        transition/audit log; SnapshotMigrator; adopt
├── xstate-eda           CloudEvents 1.0 Envelope; BrokerAdapter/SyncBrokerAdapter + fake broker;
│                        InboundDispatcher; transactional outbox (memory/SQLite + relay with leases);
│                        dead letters (memory/SQLite, broker sink, plugin, replay); AsyncAPI 3.0;
│                        broker adapters: Redis Streams, NATS (JetStream), RabbitMQ, SQS (features)
├── xstate-patterns      RetryPolicy (jitter modes), CircuitBreaker (chart-driven), SagaBuilder
│                        (emits XState JSON), ChoreographyRouter (+ PersistedHandler)
├── xstate-observability MetricsObserver over the `metrics` facade (Python PrometheusPlugin metric
│                        set), LabelGuard cardinality hygiene
├── xstate-inspect       Stately Inspector protocol (@statelyai/inspect 0.7.2): InspectorObserver,
│                        allow-lists + redaction, MemorySink/JsonLinesSink/SseSink (hardened std-only
│                        HTTP/SSE server), read_jsonl, replay
├── xstate-macros        #[derive(TypedEvent)], #[derive(TypedState)], statechart! DSL (compile-time
│                        checked), crate-path resolution via proc-macro-crate
├── xstate-axum          StatechartRegistry over a StateStore; REST + per-event routes, SSE and
│                        WebSocket streaming, RFC 9457 problem details, idempotency keys, authorizers,
│                        probes, graceful drain, Inspector WebSocket endpoint
├── xstate-agents        LLM agents as statecharts: TOOL_LOOP/supervisor/pipeline/debate charts,
│                        AgentLogic (callModel/runTool), budgets as guards, ToolRegistry, runner,
│                        AgentTrace, structured output, multi-agent helpers (no provider SDKs)
├── xstate-testing       skeleton only (testing utilities live in xstate-core::graph and the
│                        conformance crate) — see §6
├── xstate-conformance   fixture model + harness for conformance/cases (dev only)
├── xstate-py            PyO3 bindings: Python module `xstate_rs` (publish = false; maturin)
└── xsm (package name xstate-statemachine-cli, binary `xsm`)
                         validate, inspect (--live), replay, diagram, docs, simulate, paths,
                         snapshots, info
```

Key design decisions (ADRs):

- **0001 Engine semantics.** When Python and XState v5 disagree, XState wins. The eight
  Python-vs-XState divergences the suite recorded against Python 0.12.0 (exit order, parallel entry
  order, wildcard matching of engine events, cross-region targets, exit actions on completion,
  reenter into descendants, multi-target rejection, JSON-form `choose`) were fixed in the Python
  repository on branch `xstate-v5-engine-parity` (2026-11-03, unreleased), so
  `conformance/divergences.generated.md` is now empty. Snapshots and the structure hash use the
  Python/v4 event spellings (`after.*`, `done.state.*`, `done.invoke.*`, `error.platform.*`) so
  the bytes match Python.
- **0002 Two-tier context.** `DynContext = serde_json::Value` (JSON-form `assign`) plus a typed
  `C: Context` for users who want structs.
- **0003 Byte-compatible snapshots and store layouts.** Layout v4; SQLite schema v2; Redis and
  PostgreSQL layouts copied from the Python `contrib` modules.
- **0004 Sans-I/O step engine.** `start()`/`step()` return `Effect`s (timers, services, child
  actors, sends, emits); the sync and tokio interpreters execute them.
- **0005 Workspace and quality gates.** Edition 2024, MSRV 1.88 (`postgres` needs 1.94 because of
  sqlx 0.9), `unsafe_code = "forbid"`, clippy pedantic, `missing_docs`, cargo-deny, CI matrix.

## 3. How it was built (history)

Everything was built in one multi-day session by Claude (Anthropic) under the owner's direction,
phase by phase, with specialised sub-agents implementing crates in parallel and the lead agent
integrating, gating and committing. Commits are on `main` in order:

| Phase | Commit(s) | Content |
|---|---|---|
| 0 Design | `a3f0f84` | ADRs, workspace skeleton, conformance oracles (Node xstate 5.33 oracle + Python oracle), 37 conformance cases, CI skeleton |
| 1 Core | `3308aea` … `7b81661` | parser/compiler/step engine, SyncInterpreter, snapshots + hash, benchmarks + allocation pass (35–70× faster dispatch than Python at the time), tokio actor, builder, exporters, pure API, policies, observers, graph utilities |
| 2 Persistence | `c1e0c87`, `74df9d5` | StateStore, memory/file/SQLite stores, locks, `persisted()`, durable timers, scanner, idempotency, receipts, log, migration, adopt |
| 3 Observability / Inspector / CLI | `60e53f2`, `0c81518` | MetricsObserver, xstate-inspect (Python golden recordings byte-identical), xsm CLI |
| 4 EDA + patterns | `f257ea4`, `18e89c1` | envelope, brokers trait + fake, dispatcher, outbox, dead letters, AsyncAPI; retry, breaker, saga, choreography |
| 5 Typed DSL / HTTP / Redis | `d66a150`, `d277a71` | xstate-macros, xstate-axum, Redis store/inbox/log, Redis Streams broker |
| 6 Postgres / NATS / RabbitMQ / live inspector / agents | `813db24`, `24b407e` | PostgresStore/Inbox/Log, NatsBroker, RabbitMqBroker, SseSink + `xsm inspect --live` + `xsm replay`, xstate-agents; fix: `serde_json` `float_roundtrip` (deadlines were one ULP off after a round trip) |
| 7 SQS / Python bindings / facade / release prep | `a2a8fee`, `eac9a52` | SqsBroker, xstate-py, facade crate, per-crate READMEs, guides, CHANGELOG, CI/release workflows, cargo-deny clean, 0.1.0 |
| Pre-release review | `d247434` | three independent reviews (security, Rust quality, correctness/parity) and fixes — see §5 |

### How correctness was established

- **Conformance suite** (`conformance/`): 37 JSON cases, each run through the real XState v5 engine
  (Node, `conformance/tools/xstate_oracle`) and the Python library
  (`conformance/tools/generate_expected.py`), producing expected traces, Python `machine_hash` and
  Python snapshot text. `crates/xstate-conformance/tests` asserts the Rust engine matches XState on
  every case and reproduces Python's hash and snapshot (text equal modulo Python's 2-space
  indentation). `generate_expected.py --check` reports 37 cases, 0 divergent, 0 stale against the
  Python parity branch (8 divergent against released 0.12.0).
- **Golden fixtures generated from the Python library** (regenerate with
  `"<python venv>" -I conformance/tools/<script>.py`; all currently drift-free):
  `inspect_golden.py` (3 inspector recordings, byte-identical), `eda_golden.py` (envelopes, dead
  letters, AsyncAPI, Python-written SQLite outbox/DLQ files), `patterns_golden.py` (3 sagas, breaker
  chart, retry delays), `agents_golden.py` (6 agent scenarios: final context, state sequence, result,
  trace records), `export_golden.py` (72 Mermaid/PlantUML files).
- **Live cross-language tests** in both directions (Rust writes → Python reads and vice versa) for
  SQLite, Redis, PostgreSQL stores, and for Redis Streams, NATS, RabbitMQ and SQS brokers. They need the
  Docker services from §4 and the Python venv; otherwise they skip with a printed note
  (`XSM_REQUIRE_SERVICES=1` turns a skip into a failure).
- **Python bindings**: `crates/xstate-py/tests/test_bindings.py` checks snapshot text and
  structure hash equality with the pure-Python library on four conformance cases, restore in both
  directions, and the benchmark `crates/xstate-py/bench/bench.py` (12–18× faster event dispatch).

### Performance baselines

`benches/budgets.json` (criterion, `crates/xstate-core/benches/core.rs`; p50 on an i7-11850H,
quiet machine; a loaded machine measured 1.5–1.9× slower):

| Benchmark | Rust p50 | Python |
|---|---|---|
| `send_flat` | 420 ns | ~14 µs |
| `send_guard_assign` | 520 ns | ~16 µs |
| `send_nested_3` | 650 ns | ~35 µs |
| `send_parallel_2` | 565 ns | ~39 µs |
| `build_50_states` | 180 µs | not comparable |
| `snapshot_get` | 1.3 µs | ~12 µs |
| `snapshot_restore` | 9 µs | ~23 µs |

`allowed_slowdown` is 1.25 but **no automated perf gate exists yet** (the `bench-gate` CI job only
builds the benches).

## 4. Environment and how to work

### Toolchain and layout

- Windows 11, PowerShell/Git Bash; stable Rust 1.99 installed (`rust-toolchain.toml` pins the
  channel), MSRV 1.88 declared.
- Python reference library + venv: `C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine\.venv\Scripts\python.exe`
  (Python 3.14; has `xstate_statemachine` 0.12.0 importable, `redis`, `boto3`, `nats-py`,
  `aio-pika`, `sqlalchemy`+`psycopg`, `maturin`, `pytest`, `pydantic`). Always run it with `-I`.
  Tests that call Python read `XSM_PYTHON`; the default assumes the sibling-directory layout above.
- Node: `conformance/tools/xstate_oracle` (xstate 5.33.x installed under `node_modules`).
- Docker Desktop is available. Services the tests use (and the env vars they read):

  | Service | Start | Env var (default) |
  |---|---|---|
  | Redis 7 | `docker run -d --name xsm-redis -p 6379:6379 redis:7-alpine` | `XSM_REDIS_URL` (`redis://127.0.0.1:6379/`) |
  | PostgreSQL 16 | `docker run -d --name xsm-pg -e POSTGRES_PASSWORD=xsm -e POSTGRES_USER=xsm -e POSTGRES_DB=xsm -p 5432:5432 postgres:16-alpine` | `XSM_POSTGRES_URL` (`postgres://xsm:xsm@127.0.0.1:5432/xsm`) |
  | NATS 2 (JetStream) | `docker run -d --name xsm-nats -p 4222:4222 nats:2 -js` | `XSM_NATS_URL` (`nats://127.0.0.1:4222`) |
  | RabbitMQ 3 | `docker run -d --name xsm-rabbit -p 5672:5672 rabbitmq:3-alpine` | `XSM_AMQP_URL` (`amqp://guest:guest@127.0.0.1:5672/%2f`) |
  | ElasticMQ (SQS) | `docker run -d --name xsm-sqs -p 9324:9324 softwaremill/elasticmq-native` | `XSM_SQS_ENDPOINT` (`http://127.0.0.1:9324`) |

  Remove them with `docker rm -f xsm-redis xsm-pg xsm-nats xsm-rabbit xsm-sqs` when done. Three
  unrelated containers named `xsm-ci-py39-*`, `xsm-original-review-docs-1`, `xsm-ui-review-docs-1`
  belong to the owner's other projects — leave them.

### The gates (all must stay green)

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features --no-fail-fast          # add XSM_REQUIRE_SERVICES=1 with Docker up
cargo deny check
cd crates/xstate-py && "<venv python>" -m maturin develop --release && "<venv python>" -m pytest tests -q
"<venv python>" -I conformance/tools/generate_expected.py --check
"<venv python>" -I conformance/tools/export_golden.py --check
```

### Local quirks you will hit

- **Windows Application Control intermittently blocks freshly built executables and proc-macro
  DLLs** (`os error 4551`, "Permission denied", "An Application Control policy has blocked this
  file", `can't find crate for tokio_macros`). It is not a code failure. Touch a source file in that
  crate (or delete the offending artefact under `target/debug/deps`) and rebuild; the new hash runs.
- **`target/` grows very large** (it reached 83 GB during development). The dev profile now uses
  `debug = "line-tables-only"`; still, run `cargo clean` periodically. A full `--all-features`
  build + tests + docs is ~17 GB.
- `cargo test` with the default parallelism once failed with `os error 1455` (paging file); use `-j 4`.
- Bash heredocs containing apostrophes break inline Python; write helper scripts to files.
- Commit messages with backticks must go through a file (`git commit -F`).

### Conventions

Edition 2024; `#![forbid(unsafe_code)]` everywhere; clippy pedantic + `unwrap_used`/`expect_used`/
`print_*` as warnings that CI promotes to errors; every public item documented, with `# Errors` /
`# Panics` sections; files under 800 lines, functions under 50 lines; let-chains over nested ifs;
no `#[allow]` without a justifying comment; conventional commits (`feat:`, `fix:`, `docs:`,
`test:`, `perf:`, `chore:`), with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
when an AI writes the commit. Tests prefer `unwrap_or_else(|e| panic!("{e}"))`.

## 5. Known state: what was reviewed, what was fixed, what is open

### Reviews already done (commit `d247434`)

Three independent reviews (security · Rust quality · correctness/parity) found no CRITICAL issues,
6 HIGH, ~40 MEDIUM/LOW. All HIGH and most MEDIUM items are fixed:

- axum: authorization is explicit (`RegisterOptions::new(authorizer)`, no `Default`); WebSocket enforces
  per-event gates and declared events; POSTs check `Origin` and require `application/json`; WebSocket
  send timeouts; registry-wide connection cap; abortable stream sessions on shutdown; authorize before
  existence checks; subscribe before peek; prefix/limit validation; probe timeout; inspector cookie
  hardening; unknown machine → 404.
- eda/persistence: RabbitMQ `mandatory` publish; capped default dead-letter store (10 000); saturating
  attempt counters and `xsmattempt` cap; `io_timeout` on all brokers; classic-queue delivery count no
  longer trusted; mid-batch error requeue; serialised connects; `OutboxPlugin` failure reporting and
  bounded buffer; lock guards never block/panic in `Drop`; Postgres leases on server clock; redacted
  driver errors; 0600 files; `XSM_REQUIRE_SERVICES`.
- macros/patterns/agents/py: crate-path resolution (facade-only compile test); `statechart!` error
  cascade; comma rule; breaker probe release; saga prefix collisions; decorrelated retry context key;
  observers outside locks; linear scrub; tool timeout/in-flight bounds; unsupported JSON-schema keywords
  rejected; trace memory bound; replay gap cap; Python GC cycles collectable.
- docs/CI: CI installs the Python library from GitHub `main`; python-bindings and facade-only jobs;
  release depends on CI; snapshot text comparison; `xsm snapshots --stale` matches Python
  (`--stale-by`); export golden generator; README/CHANGELOG/guide corrections.

### Known gaps and deliberate deviations (verify, do not rediscover)

Engine / core
- The eight recorded Python-vs-XState divergences are fixed on the Python `xstate-v5-engine-parity`
  branch (unreleased); `conformance/divergences.generated.md` is empty. Remaining engine differences
  (not pinned by a case yet — add one each): `done.state.*` queue, conflict preemption between
  transitions selected in one microstep, guard evaluation order, `assign` ordering, compound
  self-target without `reenter` (XState resets the children; Python runs only the actions), JSON
  `assign` without an `assignment` wrapper (Python ignores it), and `invoke` without `input`
  (Python 0.12.0 briefly forwarded the triggering payload; reverted, `event_payload` is the explicit
  opt-in; XState/Rust pass `null`). See `docs/guide/migrating-from-python.md`.
- Rust engine fix found during the Python convergence: `start()` landing directly in a top-level
  final state now runs the exit actions (XState does; `crates/xstate-core/tests/start_in_final.rs`).
- `RestoreOptions::default()` resumes timers; Python's bare `from_snapshot` parks them (`persisted()`
  resumes in both).
- Mermaid export renames a node whose id is the reserved word `end` (Python emits invalid Mermaid).
- `xstate-testing` is a skeleton.

Persistence
- No outbox store for PostgreSQL (memory + SQLite only); the outbox is at-least-once, not atomic with
  the snapshot save (Rust observers cannot join the `persisted` session; the plugin buffers and the
  caller publishes in `Session::after_commit`).
- Postgres `lock()` is not re-entrant; each call is its own transaction; `version` is `INTEGER`
  (fails at 2^31, same as Python); no `text_pattern_ops` index (would break the DDL-equivalence test
  with Python).
- Redis `{p}:keys` set retains expired members until `list_keys`/`forget` prune them.
- `postgres` feature needs Rust 1.94 (sqlx 0.9); MSRV job skips it.

EDA
- No Kafka adapter (`rdkafka` needs cmake/librdkafka, absent on the build machine; pure-Rust clients
  lack consumer groups).
- Async `InboundDispatcher::run_once` runs handlers on scoped threads and acks per batch (Python:
  executor + per-subject acks).
- `DeadLetterPlugin` keeps its error chain in memory per plugin (Python keeps it in context).
- SQS tested only against ElasticMQ; the SQS cross-language test never exercises FIFO ordering
  (`ordered=false`).
- Broker options differ from Python's `on_disconnect`/`on_reconnect`/`dead_letters` hooks
  (`healthy()` + `on_undecodable` instead).

Patterns / agents
- Choreography is sync-only; the breaker has `wrap` instead of a decorator; decorrelated jitter's
  context key trails Python by one delay.
- Agents: sub-agents run to completion inside the parent's action (no long-lived children);
  `BudgetPlugin` totals sub-agent usage only when wired through `SpawnAgentConfig`; the child
  allow-list re-check at spawn time is missing; provider SDKs, `pydantic_ai`, `langgraph` not ported;
  output models are JSON schemas.

HTTP / inspector
- Timer-fired transitions are not pushed to SSE/WebSocket subscribers (no scanner observer hook).
- `Last-Event-ID` is not replayed (same as Python).
- The Inspector page forwards inspected messages to `https://stately.ai/inspect` when the "Stately
  UI" box is ticked (default on, as in Python) — documented, not changed.
- `SseSink`'s `--open` path and the Stately iframe were never checked in a real browser.

Macros
- No forbidden-transition form (`"E": null`), no `and`/`or` guard combinators, no per-field event
  renames, no generic enums, nested `TypedState` variants need explicit ids.

Python bindings
- `start()` returns a `Receipt` (Python returns the interpreter); callbacks take
  `(context, event, params)`; re-entrant sends raise; async interpreter/actors/`can()`/plugins not
  exposed; `test = false` on the Rust lib target (pytest is the test suite).

Release
- The crates.io name `xsm` is taken → package `xstate-statemachine-cli`, binary `xsm`.
- One ignored advisory (RUSTSEC-2025-0134 via lapin 3; lapin 4 breaks the adapter API).
- CI/release workflows have never executed; the integration job's Python install list is best-effort.
- No GitHub Pages site exists. The docs are Markdown under `docs/guide/`, `docs/adr/`, per-crate
  READMEs and rustdoc.

## 6. The QA mandate

Treat every claim in this document, the README, the CHANGELOG, the guides and the rustdoc as
**unverified until you have exercised it**. The goal is a release the owner can trust and a
documentation site users can rely on. Work in this order; each item ends with a written report
section (format in §8) and, where you fix something, a commit that passes all gates.

### 6.1 Build, gates and repository hygiene

- Fresh clone build on Windows **and** Linux (WSL or a container): default features, each feature
  alone, `--all-features`, `--no-default-features`, MSRV 1.88 without `postgres`, `cargo doc`,
  `cargo deny`, `cargo test --doc`. Record build times and `target/` size.
- Create the GitHub repository (`basiltt/xstate-statemachine-rs`, matching `Cargo.toml`
  `repository`), push, and make **CI green**: `ci.yml` (fmt, clippy, test matrix on three OSes,
  MSRV, integration with services, deny, docs, bench-gate, python-bindings, facade-only,
  conformance-fixtures) and a dry run of `release.yml` on a throwaway tag. Fix workflow bugs — they
  have never run.
- `scripts/publish.sh --dry-run` for all 13 publishable crates; check manifest metadata, keyword
  limits, README rendering on crates.io (use `cargo package --list` to confirm what ships; the core
  tests that read `../../conformance/cases` must skip, not fail, from a tarball).
- Licence headers/LICENSE, `CHANGELOG.md` accuracy against `git log`, CODE_OF_CONDUCT /
  CONTRIBUTING / SECURITY.md (missing — add them), issue templates, `.gitattributes` for line
  endings (several files are CRLF).

### 6.2 Functional battle-testing, crate by crate

For each crate: read the public API, write adversarial tests for every public function (invalid
input, boundary sizes, concurrency, cancellation, restart), compare against the Python
implementation's behaviour, and keep any test that found a bug. Minimum matrices:

**xstate-core**
- Every XState v5 construct through the parser: compound/parallel/final/history (shallow/deep),
  `always`, `after` (numeric and named delays, delay functions), `invoke` (sync, async, machine) with
  `onDone`/`onError`, `onDone` of compound and parallel states with output, `entry`/`exit`, guards
  (named, params, `not`/`and`/`or`, `stateIn`), actions (`assign`, `raise`, `sendTo`, `sendParent`,
  `emit`, `log`, `cancel`, `choose`, `stop`), wildcards (`*`, `a.*`), targetless and internal
  transitions, `reenter`, `#id` targets, `tags`, `meta`, `description`, `strict`, `onUnhandled`
  policies, `actionErrorPolicy`/`guardErrorPolicy`, `maxIterations` (runaway `always` chains).
- Property-based tests (proptest is a workspace dev-dep): random machines → `to_json` → `from_json`
  round trip; snapshot round trip; structure hash stability; configuration legality after random
  event sequences; no panics for arbitrary JSON input (fuzz the parser with `cargo fuzz` or a
  proptest JSON generator — the parser must never panic, only error).
- Limits: `DEFAULT_MAX_INPUT_BYTES`, deeply nested states (stack depth), thousands of states/events
  (build time, memory), huge payloads, non-ASCII ids, duplicate ids, cyclic `always`.
- Stately editor exports: build five real machines in the editor (parallel, history, invoke, guards
  with params, delays) and run them; verify `docs/guide/stately-editor.md` matches reality.
- Compare `SyncInterpreter` vs tokio `Interpreter` vs the pure API on the same inputs.

**xstate-tokio**
- Timers under `tokio::time::pause`, cancellation of delayed sends, mailbox overflow policies,
  `send_and_wait` on a finished actor, child machine lifecycles (`stop` propagation, `done.invoke`,
  `error.actor`), `System` registry collisions, panics inside services, backpressure.

**xstate-persistence**
- Each store against the `StateStore` contract with the same test suite (write a contract test
  macro if one does not exist): key validation, size caps, versioning, conflicts under 50 concurrent
  writers (optimistic and pessimistic), lock timeouts and release on panic/drop, `due_keys` ordering,
  `forget` completeness, corruption handling (truncate/garble a record), schema version refusal,
  migration paths, adoption. File store on Windows paths with reserved names and long keys.
- Durable timers end-to-end: process A arms an `after`, dies; process B scans and fires; clock skew;
  `RestartTimers` variants.
- Idempotency inbox: duplicate detection across restarts, TTL expiry, fingerprint mismatch, scope
  escaping with `/` in sources, concurrent claims.
- Cross-language with Python for SQLite, Redis, PostgreSQL (both directions, including locks held by
  the other runtime, deadlines, inbox, log) — extend beyond the existing tests (large snapshots,
  unicode keys, version races).

**xstate-eda**
- Envelope: every CloudEvents 1.0 attribute rule, extension-name grammar, `traceparent` grammar,
  size and depth limits, malformed JSON, every Python error message (the golden has 24 cases — add
  more).
- Dispatcher: ordering per subject, attempts and dead-letter reasons (`max_attempts`, `corrupt`,
  `unknown_event`, `idempotency_mismatch`, `instance_done`), replay (dry run, forced, audited),
  panics in handlers, batch/in-flight limits, behaviour when the inbox or DLQ store fails.
- Outbox: relay leases under concurrent relays, crash between claim and publish, lease expiry,
  `max_buffered`, failure surfacing; Python-written SQLite outbox read by Rust and back.
- Brokers (with Docker): each adapter through the same behavioural suite (publish/poll/ack/nack,
  redelivery, ordering, consumer groups/queues shared by two consumers, oversized and undecodable
  messages, `io_timeout` against a silent TCP server, reconnect after the container restarts, FIFO
  for SQS, JetStream dedup for NATS, quorum queues for RabbitMQ), plus Python interop in both
  directions. Measure throughput and latency per adapter.
- AsyncAPI: validate generated documents with an external AsyncAPI 3.0 validator (e.g.
  `@asyncapi/parser` via Node) — the in-crate check is structural only.

**xstate-patterns / xstate-agents**
- Retry: delay bounds per jitter mode over 10k samples; `apply` into a real machine with
  `SimulatedClock`; context key behaviour vs Python.
- Breaker: threshold/cooldown/half-open under 100 threads; ignored/filtered probes; reset; observer
  reentrancy.
- Saga: goldens plus generated sagas with 20 steps; compensation ordering; timeouts at runtime via the
  tokio interpreter (the sync interpreter cannot wait).
- Choreography: two machines ping-ponging, unknown types, loop guard.
- Agents: all six golden scenarios plus adversarial tool arguments (schema violations, huge strings,
  prompt-injection in tool outputs reaching the trace), budget exhaustion on each dimension,
  approvals timing out (`awaiting_human` escalation), secrets scrubbing coverage, trace sinks (file
  0600 on Unix, memory bound), multi-agent handoffs.

**xstate-observability**
- Every metric name/label against Python's Prometheus output; cardinality guard under a flood of
  undeclared events; `timed`.

**xstate-inspect**
- Golden recordings extended with children, `after`, errors, payload allow-lists; `SseSink`
  security suite (already 24 tests — add fuzzing of the HTTP parser with random bytes, slowloris,
  128+ connections, header injection attempts); replay timing; **real browser check** of the page and
  the Stately iframe (`xsm inspect --live`), on Windows and Linux.

**xstate-macros**
- Every DSL construct; compile-fail coverage for every validation rule (currently 8 UI cases);
  generated code under `#![deny(warnings)]` and `clippy::pedantic` in a user crate; facade-only
  crate; renamed dependencies; `rename_all` conventions; large charts (recursion limits).

**xstate-axum**
- Full HTTP contract suite: every route × (authorized, forbidden, unauthenticated), problem-details
  shape, status table vs Python (`contrib/starlette/_http.py`), idempotency (replay, mismatch,
  in-flight, TTL), body limits (`Content-Length` lies, chunked bodies), content types, `Origin`
  rules, per-event gates on all three entry points, SSE framing and heartbeats, WebSocket close
  codes, fan-out under 1000 subscribers, slow consumers, global/per-key caps, graceful drain with open
  streams, probes, inspector endpoint token/cookie/Host/Origin rules. Load test with `oha`/`wrk`
  (p99 latency, connections, memory). Run an OWASP-style checklist (injection, auth bypass, SSRF,
  DoS, information leakage in errors/logs).

**xstate-py**
- API parity table vs the Python `SyncInterpreter`; exceptions mapping; GC/leak test over 1M sends;
  thread-safety (two threads driving two interpreters, GIL release); benchmark reproduction; wheel
  build for manylinux/macOS/Windows via maturin in CI; `mypy --strict` against the stubs.

**xsm CLI**
- Every subcommand and flag against the Python CLI's behaviour and exit codes (0/1/2), `--json`
  schemas, colour/`NO_COLOR`, large files, invalid UTF-8, Windows path quirks, `snapshots` on every
  store URL form, `simulate` scripts, `--record` → `xsm replay` round trip, `inspect --live` driven
  over stdin.

### 6.3 Non-functional

- **Performance**: run the criterion suite on a quiet machine, compare to `benches/budgets.json`,
  and build the missing **perf gate** (a script or `xtask` that fails CI when p50 regresses beyond
  `allowed_slowdown`). Add benches for persistence stores, dispatcher, envelope encode/decode,
  inspector encoding, macros-generated machines. Profile allocations on `send` (the earlier
  allocation pass used `allocprof`; `dhat` or `heaptrack` on Linux). Compare against the Python
  benchmarks (`benchmarks/competitors` in the Python repo) and update the README table with
  reproducible numbers and the exact command.
- **Memory and resource leaks**: long-running actors, dispatcher with 1M envelopes, SSE server with
  churned clients, Python bindings.
- **Error handling**: every `Error` variant reachable and documented; no `Other(String)` where a
  typed variant should exist; errors never leak secrets (hosts, tokens, payloads); panics only where
  documented (`# Panics`), none reachable from untrusted input.
- **Logging/tracing**: targets (`xstate`, `xstate::persistence`, `xstate::inspect`, …) consistent;
  levels sensible; no payload/context in logs by default; a `tracing-subscriber` example.
- **Security**: re-run the §5 security review scope with fresh eyes; `cargo audit`; dependency
  review (why each dependency, unmaintained crates); supply-chain (lockfile committed, `--locked` in
  CI); secrets scanning of the history; threat model document for the HTTP surfaces and the SSE
  inspector.
- **Industry standards**: XState v5 conformance (the Node oracle is the arbiter), W3C SCXML Appendix
  D algorithm, CloudEvents 1.0 spec, AsyncAPI 3.0 schema, RFC 9457 problem details, RFC 6455 WebSocket
  close codes, SSE (WHATWG) framing, OWASP ASVS for the HTTP layer, Keep a Changelog, Semantic
  Versioning (0.1.0 → what is a breaking change?), Rust API Guidelines checklist
  (`C-*` items: naming, `#[non_exhaustive]`, `Debug`/`Clone`/`Send`/`Sync` on public types,
  builder conventions, error types implementing `std::error::Error` + `Send + Sync`), MSRV policy
  statement.
- **Code standards**: `cargo clippy -- -W clippy::nursery -W clippy::cargo` as advisory;
  `cargo udeps`, `cargo machete` (unused deps), `cargo semver-checks` baseline, `cargo
  llvm-cov` coverage report per crate (target ≥ 80%; identify untested public functions),
  `cargo geiger` (should be zero unsafe), `typos`, `cargo doc` with `--document-private-items`
  to catch broken private links, file/function size limits.

### 6.4 Documentation and the GitHub Pages site ("the bible")

The site does not exist yet. Build it with **mdBook** (`docs/book/`, published by a
`pages.yml` workflow to `https://basiltt.github.io/xstate-statemachine-rs/`), with rustdoc for all
crates published under `/api/` from `cargo doc --workspace --all-features --no-deps`. Requirements:

1. **Structure**: Introduction (why, what, status, parity with Python) · Getting started (install,
   first machine, Stately editor export, typed DSL) · Concepts (statecharts, XState v5 semantics,
   the sans-I/O engine, events, context, guards, actions, delays, invoke, actors, snapshots, structure
   hash, policies) · Guides per feature (sync interpreter, tokio actors, persistence per store, locks,
   durable timers, idempotency, log, migration, EDA envelope/dispatcher/outbox/dead letters/AsyncAPI,
   each broker, patterns, observability, inspector incl. live/replay, CLI reference with every
   command/flag/exit code, HTTP integration with the full route table and problem types, agents,
   macros reference with the complete grammar, Python bindings) · Operations (deployment, scaling,
   security hardening checklist, monitoring, backup/restore of stores, migrating machines) ·
   Interoperability with Python (what is byte-compatible, how to share stores/brokers, migration
   guide) · Cookbook / examples · Reference (feature flags, env vars, error catalogue with every
   error variant and problem type, metric catalogue, event name spellings, divergences from Python
   and XState, MSRV/semver policy, FAQ, glossary) · Contributing (gates, conformance suite,
   regenerating goldens, adding a store/broker) · Changelog.
2. **Examples of every possible case**: each concept page needs runnable examples; create
   `examples/` crates (a `cargo run --example` per scenario: traffic light, order saga with outbox,
   choreography over the fake broker, persisted workflow resuming after a crash, durable timers, HTTP
   service with axum, agent tool loop with `FakeModel`, inspector recording and replay, typed DSL,
   Python bindings script) and `mdbook-test`/doctests so every snippet compiles in CI. Cover error
   paths too (what the user sees when a guard is missing, a snapshot drifts, a broker is down).
3. **Verification**: every page cross-checked against the code (names, flags, defaults, status
   codes); link checker (`mdbook-linkcheck`/`lychee`) in CI; rustdoc intra-doc links; search works;
   renders on mobile; versioned docs plan.
4. Existing material to fold in and correct where wrong: `README.md`, `docs/guide/*.md`,
   `docs/adr/*.md`, `docs/feasibility-report.md` (historical), per-crate READMEs, `conformance/README.md`,
   `benches/README.md`, CLI `--help` text, rustdoc crate headers.

### 6.5 Python parity audit

Walk the Python package module by module (`src/xstate_statemachine/**`, including `contrib/`) and
produce a table: Python symbol → Rust equivalent → parity (same / documented difference / missing).
Appendix A of `docs/feasibility-report.md` and `docs/guide/migrating-from-python.md` are the
starting points. Missing items known today: Kafka adapter, SQLAlchemy outbox/model store/mixin, Celery,
Django/DRF/Flask/Quart/Litestar/Starlette/FastAPI adapters beyond axum, pydantic models, agents'
provider integrations, Python-only CLI commands (`list-templates`), `xsm` interactive TUI.

## 7. Suggested order of work

1. Fresh-clone builds on both OSes; create the GitHub repo; make CI and a release dry run green;
   publish the Pages skeleton (so the site URL exists from day one).
2. Coverage report + contract test suites for stores and brokers (cheap, finds the most).
3. Engine fuzzing and property tests; Stately editor exports.
4. HTTP security/load suite; inspector browser verification.
5. Performance gate and benchmark reproduction; README numbers.
6. Agents, patterns, macros adversarial tests.
7. Documentation site to completion with examples; parity audit table.
8. Final release checklist (§8) and the 0.1.0 tag.

## 8. Reporting and fixing

- Keep a running `docs/qa/REPORT.md` (create `docs/qa/`): one section per area, each finding with
  **severity** (CRITICAL: security hole or data loss · HIGH: wrong behaviour, false claim, panic from
  input · MEDIUM: limited-impact defect, missing test, misleading doc · LOW: polish), file:line, a
  reproduction (test name or command), the fix (commit hash) or the reason it was deferred, and a
  "verified sound" list per area so the owner knows what was checked.
- Fix CRITICAL and HIGH yourself when the fix is local and covered by a test; for behaviour changes
  that alter public API or Python parity, write the proposal in the report and ask the owner.
- Every commit passes all gates in §4; one logical change per commit; conventional commit messages;
  update `CHANGELOG.md` under `Unreleased`.
- Never weaken a test to make it pass; never skip a golden; if a golden must change, regenerate it
  from Python with the script and explain why in the commit.
- Do not push to crates.io or PyPI. Tagging `v0.1.0` is the owner's call; prepare everything up to it.

## 9. Final release checklist

- [ ] CI green on main for three OSes, MSRV, integration, deny, docs, python-bindings, facade-only, fixtures
- [ ] `release.yml` dry run green on a `v0.1.0-rc` tag; `scripts/publish.sh --dry-run` green
- [ ] Coverage ≥ 80% per crate; fuzz targets run ≥ 1 h each without findings
- [ ] Perf gate in CI; README numbers reproduced with the stated command
- [ ] Security review re-run; threat model and SECURITY.md published; `cargo audit` clean
- [ ] Parity table published; every known gap listed in the docs
- [ ] GitHub Pages live, link-checked, every page verified against code, examples compile in CI
- [ ] CHANGELOG 0.1.0 final; versions, metadata and READMEs render on crates.io dry run
- [ ] Owner sign-off

## Appendix A. File map of things you will look for

| What | Where |
|---|---|
| Feasibility study and roadmap | `docs/feasibility-report.md` |
| Design decisions | `docs/adr/000{1..5}-*.md` |
| Guides | `docs/guide/{getting-started,stately-editor,persistence,eda,migrating-from-python,features}.md` |
| Conformance cases, schema, oracles, divergences | `conformance/{cases,schema,tools,divergences.generated.md,README.md}` |
| Golden generators | `conformance/tools/{generate_expected,inspect_golden,eda_golden,patterns_golden,agents_golden,export_golden}.py` |
| Benchmarks and budgets | `crates/xstate-core/benches/core.rs`, `benches/{budgets.json,budgets.schema.json,README.md}` |
| CI / release | `.github/workflows/{ci,release}.yml`, `scripts/{publish.sh,publish-order.txt}`, `deny.toml` |
| Facade features | `crates/xstate-statemachine/Cargo.toml` (`tokio persistence sqlite redis postgres eda redis-streams nats rabbitmq sqs patterns observability inspect macros axum agents testing full`) |
| Python bindings | `crates/xstate-py/{src,python,tests,bench,pyproject.toml}` |
| CLI | `crates/xsm/src/{main.rs,commands/*.rs}`, tests `crates/xsm/tests/` |
| Python reference | `C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine\src\xstate_statemachine\` |

## Appendix B. Glossary

- **Structure hash / `machine_hash`** — SHA-256 over a Python-`repr`-compatible rendering of the
  chart structure; identical across runtimes; used to detect snapshot drift.
- **Snapshot layout v4** — the JSON document stores persist (`v`, `machine_id`, `machine_hash`,
  `status`, `context`, `state_ids`, `value`, `configuration`, `deadlines`, `history`, …).
- **Receipt** — what `send` returns: state ids, `changed`, `unhandled`, actions, status, error.
- **Effect** — an I/O request produced by the sans-I/O engine (timer, service, child, send, emit).
- **persisted()** — load → body → save cycle under a lock strategy with `after_commit` callbacks.
- **Envelope** — CloudEvents 1.0 event with the `xsmattempt` extension.
- **Golden** — a fixture generated by the Python library that a Rust test must reproduce.
- **Oracle** — the Node xstate engine or the Python library used to generate expectations.
