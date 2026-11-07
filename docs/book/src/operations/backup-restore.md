# Backup and restore

Persistence stores snapshots as layout-v4 JSON plus per-store indexes. The backup story is the
store backend's own, because that is where the durable copy lives.

## What is stored

Each store implements the same `StateStore` contract (`load`, `save` with `expected_version`,
`delete`, `forget`, `list_keys`, `lock`, `health`, `due_keys`), plus the durable-timer deadlines,
the idempotency inbox, the transition log, and (in `xstate-eda`) the outbox and dead-letter rows.
The unit that matters is one machine *instance* (a key): its snapshot and its deadline.

## Backing up per store

| Store | Feature | Backup |
|---|---|---|
| `FileStore::open(dir)` | always | copy the directory of `.xsm.json` records (plus `.lock` files); records are atomic-renamed |
| `SqliteStore::open(path)` | `sqlite` | copy the database file — it uses WAL by default, so use SQLite's online backup or a checkpointed copy, not a raw `cp` mid-write |
| `RedisStore` | `redis` | the backend's own persistence (RDB/AOF), or `SCAN` the `snap:{key}` hashes |
| `PostgresStore` | `postgres` | `pg_dump` of the `xsm_*` tables |

The `xsm snapshots` command inspects and sanity-checks a store from the CLI:

```bash
xsm snapshots --store sqlite:///app.db checkout.json            # list records and their labels
xsm snapshots --store sqlite:///app.db checkout.json --stale    # records whose machine_version differs
xsm snapshots --store sqlite:///app.db checkout.json --stale --stale-by hash   # structure changed
xsm snapshots --store file:///var/lib/app/states --fail-if-stale checkout.json # CI / deploy gate
```

Only `sqlite:///` and `file:///` stores are readable by the CLI.

## Restoring and verifying

Restore is `SyncInterpreter::from_snapshot(...)` or the `persisted` cycle; the snapshot's
`machine_hash` is checked against the machine before restore, so a record written for a different
machine structure is refused rather than silently mis-restored. That check is your deploy gate:
run `xsm snapshots ... --stale --stale-by hash` against the store before shipping a new machine
`version`.

## Migrating between machine versions

When a machine definition changes, `SnapshotMigrator` registers upgrade steps between versions (a
breadth-first search finds the path), wired through
`PersistedOptions::on_version_mismatch` as `VersionMismatch::Migrate(f)`. The other policies are
`Error` (the default) and `Warn`. `from_state_ids` builds a restorable layout-v4 snapshot from the
bare state ids of an existing row, to move a hand-rolled state column onto a machine.

See the [persistence guide](../guide/persistence.md) for the full model.
