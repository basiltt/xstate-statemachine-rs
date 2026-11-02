# xstate-persistence

Persistence for `xstate-core` machines: the `StateStore` contract, memory, file and SQLite
stores (plus Redis and PostgreSQL behind features), optimistic and pessimistic locking with the
`persisted` transaction wrapper, durable `after` deadlines and their scanner, the idempotency
inbox, the transition / audit log, snapshot migrations and adoption of existing records.

All layouts are byte-compatible with the Python library (ADR-0003): a SQLite file, a directory of
`.xsm.json` records, a Redis keyspace or a PostgreSQL schema can be shared between the two
runtimes, and a Rust worker can resume an instance a Python worker saved.

Status: 0.1.0. Stores are blocking; the Postgres backend drives sqlx on an owned runtime.

```rust
use xstate_core::{Machine, MachineLogic};
use xstate_persistence::{MemoryStore, PersistedOptions, persisted};

let machine = Machine::from_json(
    r#"{"id":"order","initial":"pending","states":{"pending":{"on":{"PAY":"paid"}},"paid":{}}}"#,
)?;
let store = MemoryStore::new();
let ids = persisted(&store, "order-1", &machine, &MachineLogic::dynamic(),
                    &PersistedOptions::default(), |interp, _session| {
    interp.send("PAY")?;
    Ok(interp.state_ids())
})?;
assert_eq!(ids, ["order.paid"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

| Feature | Adds |
|---|---|
| `sqlite` (default) | `SqliteStore`, `SqliteInbox`, `SqliteLog` (bundled SQLite) |
| `redis` | `RedisStore`, `RedisInbox`, `RedisLog` (Lua compare-and-set, Python key layout) |
| `postgres` | `PostgresStore`, `PostgresInbox`, `PostgresLog` (Python SQLAlchemy schema) |

## Python counterpart

[`xstate_statemachine.persistence`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/persistence)
and `contrib.redis` / `contrib.sqlalchemy`.
