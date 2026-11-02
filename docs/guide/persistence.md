# Persistence

`xstate-persistence` (feature `persistence` of the facade) saves and resumes machine instances.
It follows the Python library's design, and its on-disk and on-wire layouts are the Python
library's too (ADR-0003): a SQLite file, a directory of `.xsm.json` records, a Redis keyspace or
a PostgreSQL schema can be shared by Rust and Python workers, in both directions. Cross-language
tests (`XSM_PYTHON`, see the [README](../../README.md#development)) verify this against the real
Python library.

## The model: create, act, persist, discard

A request-driven service does not keep interpreters alive. For every unit of work it loads the
snapshot for a key, restores an interpreter, sends events, saves the new snapshot (with a
version check) and drops the interpreter. `persisted` is that cycle as a closure:

```rust
use xstate_statemachine::persistence::{PersistedOptions, SqliteStore};
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(&std::fs::read_to_string("checkout.json")?)?;
let logic = MachineLogic::dynamic(); // + the names the chart uses
let store = SqliteStore::open("app.db")?;            // or SqliteStore::open_in_memory()
let options = PersistedOptions::default();

// First request: no record for the key yet, so a fresh machine is started and saved.
persisted(&store, "order-1", &machine, &logic, &options, |interp, _session| {
    interp.send("ADD")?;
    Ok(())
})?;

// A later request, possibly another process, thread or a Python worker.
let state = persisted(&store, "order-1", &machine, &logic, &options, |interp, _session| {
    interp.send("PAY")?;
    Ok(interp.state_ids())
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

What `persisted` does, in order: take the lock of the configured strategy; load the record and
restore the interpreter (or start a fresh machine when the key is unknown and
`create_if_missing` is set); verify the structure hash of the stored snapshot against the machine
(`verify_hash`); run your closure once; if it returned `Ok`, save the snapshot with the version
the strategy fences on; release the lock; then run the `session.after_commit(..)` callbacks.
If the closure returns `Err`, nothing is written.

Restoring does **not** re-run entry actions; pending `after` timers are handled by
`restart_timers` (`Resume` by default: remaining time is honoured against the wall clock).

## Stores

| Store | Feature | Layout shared with Python |
|---|---|---|
| `MemoryStore` | always | n/a (tests, single process) |
| `FileStore::open(dir)` | always | one `.xsm.json` per key, percent-encoded names, atomic rename, advisory file locks |
| `SqliteStore::open(path)` | `sqlite` | `statecharts` and `deadlines` tables, schema v2, WAL by default |
| `RedisStore::open(url, &RedisStoreOptions::new("app"))` | `redis` | `snap:{key}` hashes, Lua compare-and-set and lock scripts, deadline index |
| `PostgresStore::connect(url, &PostgresStoreOptions::default())` | `postgres` | the SQLAlchemy schema (`xsm_snapshots`, `xsm_deadlines`, `xsm_locks`, `xsm_inbox`, `xsm_transitions`, `xsm_schema`) |

All implement `StateStore`: `load`, `save` (with `expected_version`), `delete`, `forget` (erase
everything about a key), `list_keys`, `lock`, `health`, `due_keys`. The trait is synchronous;
from async code call it inside `tokio::task::spawn_blocking`. Records are capped at 1 MiB by
default, keys are validated, and a store or snapshot with an unsupported schema version is refused.

## Concurrency

Two strategies decide how concurrent writers coordinate:

- `OptimisticLock` (the default): no lock is held; the save fails with `StoreError::Conflict` if
  someone else saved in between. `persisted_retry` re-runs the closure with a jittered backoff
  (`OptimisticLock { retries: 5, .. }`), so the closure must be free of side effects other than on
  the interpreter, or use `session.after_commit` for them.
- `PessimisticLock { timeout }`: the store's per-key lock is held for the whole cycle. On Redis
  the lock has a TTL, which is why the save is fenced by the version as well.
- `NoLock` for single-writer setups.

```rust
use xstate_statemachine::persistence::{
    MemoryStore, OptimisticLock, PersistedOptions, persisted_retry,
};
# use xstate_statemachine::prelude::*;
# let machine = Machine::from_json(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#)?;
# let logic = MachineLogic::dynamic();
let store = MemoryStore::new();
let options = PersistedOptions {
    lock: Box::new(OptimisticLock { retries: 3, ..OptimisticLock::default() }),
    ..PersistedOptions::default()
};
persisted_retry(&store, "m-1", &machine, &logic, &options, |interp, session| {
    interp.send("GO")?;
    session.after_commit(|| Ok(())); // runs once, after the save succeeded
    Ok(())
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Durable timers

An `after: 24h` on an interpreter that is discarded after every request would never fire. The
deadline is therefore stored in the snapshot (`deadlines`) and indexed by the store.
`DueTimerScanner` finds matured deadlines and wakes their machines under a lock with
`RestartTimers::FireDue`; run it from a scheduler (`run_once`) or a loop (`run_until`). A timer
fires no earlier than its deadline and no later than the next scan after it. A crash between
firing and saving fires again on the next scan (at-least-once), so make the transition
idempotent or pair it with the inbox.

## Idempotency inbox and receipts

`IdempotencyGuard` over an `InboxStore` (`MemoryInbox`, `SqliteInbox`, `RedisInbox`,
`PostgresInbox`) implements the HTTP `Idempotency-Key` semantics of the Python library:
the first delivery claims the key (`Admit`), a repeat gets the stored answer (`Duplicate`), one
arriving while the first is still running is `InFlight` (HTTP 409), and the same key used for a
different event is a `Mismatch` (HTTP 422). Keys are scoped by principal. `ReceiptJson` is the JSON receipt (`state_ids`,
`changed`, `error`, `deferred`, `denied`, `duplicate`) with the HTTP status mapping used by
`xstate-axum`.

## Transition log

`TransitionLogObserver` records every transition as a `TransitionRecord` into a
`TransitionLogStore`: `MemoryLog`, `JsonLinesLog`, `SqliteLog`, `RedisLog`, `PostgresLog`, with
key-based redaction of payloads. Attach it with `interp.observe(..)` or `PersistedOptions::observers`.

## Migrations and adopting existing data

- `SnapshotMigrator` registers steps between machine versions (a breadth-first search finds the
  path). Plug it in through `PersistedOptions::on_version_mismatch` as
  `VersionMismatch::Migrate(upcast_fn)`; the other policies are `Error` (default) and `Warn`.
- `from_state_ids` builds a restorable layout-v4 snapshot from the bare state ids of an existing
  row, to move a hand-rolled state column onto a machine.

## Operating it

```bash
xsm snapshots --store sqlite:///app.db checkout.json --stale   # records written by another machine version
xsm snapshots --store file:///var/lib/app/states --fail-if-stale checkout.json   # CI / deploy gate
```

Backends other than memory, file and SQLite are tested against live servers; see the
[development section](../../README.md#development) for the `XSM_REDIS_URL` and `XSM_POSTGRES_URL`
variables and how to start them with Docker.
