# Ecosystem Integrations

`xstate-statemachine` is architected to integrate seamlessly with the most popular, high-download crates in the Rust ecosystem.

The core engine (`xstate-core`) maintains **zero external heavy dependencies** and executes with sub-microsecond latency. Everything that connects to a web framework, database ORM, or message broker is decoupled into modular crates or feature flags.

---

## 🗺️ Pick Your Path

Find the optimal combination for your architecture:

```mermaid
flowchart TD
    W{Web Framework?}
    W -->|Axum| AX["Axum (xstate-axum)<br/>REST · SSE · WebSockets"]
    W -->|Actix-web| AC["Actix-web<br/>web::Data · Actor Bridge"]
    W -->|Tower| TW["Tower Service & Layer<br/>State Guard · Rate Limiter"]
    W -->|None / CLI / Embedded| DB

    AX --> DB{Persistence?}
    AC --> DB
    TW --> DB

    DB -->|SQLx (Async SQL)| SQLX["PostgreSQL · SQLite<br/>(xstate-persistence)"]
    DB -->|SeaORM (Async ORM)| SEA["SeaORM ActiveModel<br/>Entity State Transitions"]
    DB -->|Diesel (Sync ORM)| DIE["Diesel Compile-Time ORM<br/>ToSql / FromSql Mappings"]
    DB -->|Redis| RD["Redis & Redis Streams<br/>Distributed Redlock"]
    DB -->|In-Memory / None| EV

    SQLX --> EV{Event-Driven?}
    SEA --> EV
    DIE --> EV
    RD --> EV

    EV -->|NATS / RabbitMQ / SQS| BRK["Message Brokers (xstate-eda)<br/>CloudEvents 1.0 · Outbox"]
    EV -->|Internal Tokio Channels| TK["Tokio Actors & Workers<br/>Bounded Mailbox Backpressure"]
```

---

## 📦 High-Download Rust Packages Supported

| Crates.io Package | Downloads | Ecosystem Role | Integration Guide | Feature Flag / Crate |
|:---|:---|:---|:---|:---|
| [`axum`](https://crates.io/crates/axum) | 35M+ | Tokio web framework | [Axum Guide](axum.md) | `xstate-axum` / `features = ["axum"]` |
| [`actix-web`](https://crates.io/crates/actix-web) | 40M+ | High-throughput web framework | [Actix-web Guide](actix.md) | Standalone pattern / adapter |
| [`tower`](https://crates.io/crates/tower) | 70M+ | Universal networking middleware | [Tower Middleware](tower.md) | Universal service layer |
| [`sqlx`](https://crates.io/crates/sqlx) | 45M+ | Async SQL toolkit (PG, SQLite) | [SQLx Guide](sqlx.md) | `features = ["sqlite", "postgres"]` |
| [`sea-orm`](https://crates.io/crates/sea-orm) | 15M+ | Async relational ORM | [SeaORM Guide](sea-orm.md) | ORM ActiveModel adapter |
| [`diesel`](https://crates.io/crates/diesel) | 30M+ | Compile-time synchronous ORM | [Diesel Guide](diesel.md) | Custom SQL mapping types |
| [`redis`](https://crates.io/crates/redis) | 35M+ | Caching, locks & streams | [Redis Guide](redis.md) | `features = ["redis", "redis-streams"]` |
| [`tracing`](https://crates.io/crates/tracing) | 150M+ | Diagnostics & structured logging | [Tracing Guide](tracing.md) | `TracingPlugin` (core) |
| [`metrics`](https://crates.io/crates/metrics) | 25M+ | Metrics facade (Prometheus) | [Prometheus Guide](prometheus.md) | `features = ["observability"]` |
| [`validator`](https://crates.io/crates/validator) | 20M+ | Schema & invariant validation | [Validation Guide](validation.md) | Pre-transition guard plugin |
| [`proptest`](https://crates.io/crates/proptest) | 30M+ | Property-based testing | [Testing Harness](testing.md) | `features = ["testing"]` |
| [`tokio`](https://crates.io/crates/tokio) | 180M+ | Async runtime & actors | [Workers & Queues](workers.md) | `features = ["tokio"]` |

---

## ⚡ Cross-Language Parity with Python

If you are migrating or operating a polyglot fleet with Python [`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine):

* `xstate-axum` mirrors `xstate_statemachine.contrib.fastapi` and `starlette`.
* `xstate-persistence` mirrors `xstate_statemachine.persistence.sqlite` and `postgres`.
* `xstate-eda` mirrors `xstate_statemachine.contrib.brokers` (NATS, RabbitMQ, SQS, CloudEvents).
* `xstate-observability` mirrors `xstate_statemachine.contrib.observability`.
* `xstate-agents` mirrors `xstate_statemachine.contrib.agents` (LangGraph / PydanticAI tool loop guards).
