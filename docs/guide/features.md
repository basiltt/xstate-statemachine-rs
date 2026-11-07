# Feature flags

Cargo features across the workspace. The engine (`xstate-core`) has one build feature,
`fast-path` (on by default), and is otherwise the same small, I/O-free crate. Backends and
integrations are opt-in so that a program pays, in compile time and dependencies, only for what
it uses, which is the same contract as the Python library's
`pip install "xstate-statemachine[extra]"`.

## The facade: `xstate-statemachine`

Depend on the facade and pick features; each one enables an optional dependency, adds a module of
the same name (`xstate_statemachine::tokio`, `::persistence`, ...) and the matching items of
`xstate_statemachine::prelude`. Nothing is enabled by default.

| Feature | Enables | Implies | Python extra |
|---|---|---|---|
| `tokio` | `xstate-tokio`: async interpreter, `spawn`, `Interpreter` | | core (asyncio engine) |
| `persistence` | `xstate-persistence`: `StateStore`, memory and file stores, locks, `persisted`, durable timers, inbox, log, migrations | | core (persistence) |
| `sqlite` | `SqliteStore`, `SqliteInbox`, `SqliteLog` (and the SQLite outbox and dead-letter stores of `eda`, if enabled) | `persistence` | core (stdlib `sqlite3`) |
| `redis` | `RedisStore`, `RedisInbox`, `RedisLog` | `persistence` | `redis` |
| `postgres` | `PostgresStore`, `PostgresInbox`, `PostgresLog` (sqlx, rustls) | `persistence` | `sqlalchemy` |
| `eda` | `xstate-eda`: envelope, broker contract, dispatcher, outbox, dead letters, AsyncAPI | `persistence` | `eda` |
| `redis-streams` | `RedisStreamsBroker` | `eda` | `redis` |
| `nats` | `NatsBroker` (JetStream) | `eda` | `nats` |
| `rabbitmq` | `RabbitMqBroker` | `eda` | `rabbitmq` |
| `sqs` | `SqsBroker` | `eda` | `sqs` |
| `patterns` | `xstate-patterns`: retry, circuit breaker, saga, choreography | `eda` | core (`patterns`) |
| `observability` | `xstate-observability`: `MetricsObserver` | | `observability` |
| `inspect` | `xstate-inspect`: Stately Inspector protocol, JSONL, replay, SSE sink | | core (`inspect`) |
| `macros` | `xstate-macros`: `TypedEvent`, `TypedState`, `statechart!` | | the Pythonic DSL |
| `axum` | `xstate-axum`: registry, REST, SSE, WebSocket, inspector endpoint | `persistence`, `inspect` | `starlette`, `fastapi` |
| `agents` | `xstate-agents`: LLM agents as statecharts | | `agents` |
| `testing` | `xstate-testing` (a skeleton today; see its README) | | `testing` |
| `full` | all of the above | | `all` |

Examples:

```toml
# a service that persists to PostgreSQL and consumes from NATS
xstate-statemachine = { version = "0.1", features = ["tokio", "postgres", "nats", "observability"] }

# a library that only needs the engine: no features
xstate-statemachine = "0.1"
```

A few points:

- `sqlite` is separate from `persistence` on purpose: it compiles a bundled copy of SQLite (a C
  dependency). `persistence` alone gives the memory and file stores.
- The Redis, PostgreSQL and SQS features pull in sizeable dependency trees (redis, sqlx, the AWS
  SDK); `full` builds all of them.
- Features are additive and unify across a build, like every Cargo feature.

## Per-crate features

If you depend on the individual crates, these are all of their features. A crate not listed has
none.

### `xstate-core`

| Feature | Default | Adds |
|---|---|---|
| `fast-path` | yes | the single-transition microstep fast path (on unless `default-features = false`) |

`--no-default-features` compiles the fast path out, so every step goes through the general
microstep. The conformance suite runs both ways in CI; the two paths are also compared
case-by-case by the crate's own differential tests.

### `xstate-persistence`

| Feature | Default | Adds | Dependencies |
|---|---|---|---|
| `sqlite` | yes | `SqliteStore`, `SqliteInbox`, `SqliteLog` | `rusqlite` (bundled SQLite) |
| `redis` | no | `RedisStore`, `RedisInbox`, `RedisLog` | `redis` (tokio, connection manager, scripts) |
| `postgres` | no | `PostgresStore`, `PostgresInbox`, `PostgresLog` | `sqlx` (rustls, postgres), `tokio` |

### `xstate-eda`

| Feature | Default | Adds | Dependencies |
|---|---|---|---|
| `sqlite` | yes | `SqliteOutboxStore`, `SqliteDeadLetterStore`; also enables `xstate-persistence/sqlite` | `rusqlite` |
| `redis-streams` | no | `RedisStreamsBroker` | `redis`, `tokio` |
| `nats` | no | `NatsBroker` | `async-nats`, `futures-util`, `tokio` |
| `rabbitmq` | no | `RabbitMqBroker` | `lapin`, `tokio` |
| `sqs` | no | `SqsBroker` | `aws-config`, `aws-sdk-sqs`, `tokio` |

`xstate-patterns` depends on `xstate-eda` and `xstate-persistence` with default features off, so
it does not drag SQLite in.

### Crates without features

`xstate-tokio`, `xstate-testing`, `xstate-inspect`, `xstate-observability`,
`xstate-patterns`, `xstate-macros`, `xstate-axum`, `xstate-agents` and the `xsm` binary (package `xstate-statemachine-cli`). The
`xstate-conformance` (test harness) and `xstate-py` (Python bindings) crates are not published to
crates.io.

## TLS and C dependencies

TLS, where a feature has it, is rustls; `openssl-sys` is banned in `deny.toml`. `sqlite` compiles a
bundled copy of SQLite, and the rustls backends of `postgres`, `nats`, `rabbitmq` and `sqs` build
`ring` or `aws-lc-sys`, so a C compiler is needed on the build machine for those features.

## MSRV

The workspace declares Rust 1.88. Every feature builds on 1.88 **except `postgres`** (and therefore
`full`, which includes it): `sqlx` 0.9 requires Rust 1.94, so enabling it needs a toolchain that
new. The `MSRV` job in `ci.yml` checks the workspace and every other feature on 1.88 and skips
`postgres`; the other jobs run on stable, which covers it.
