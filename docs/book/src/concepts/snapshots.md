# Snapshots and the structure hash

A snapshot captures everything needed to resume an instance: layout v4 with `version`, `machine_id`,
`machine_hash`, `machine_version`, `taken_at`, `status`, `context`, `state_ids`, `configuration`,
`history`, `output`, `error`, pending and deferred events, deadlines, actors and the rest.

`SyncInterpreter::snapshot()` produces one; `SyncInterpreter::from_snapshot(...)` restores it with
a `RestoreOptions` (see also `persisted` in the [persistence guide](../guide/persistence.md)).

## The structure hash

`machine_hash` is the first 16 hex chars of SHA-256 over the canonical, sorted-key JSON of the
per-node structural shape — the same algorithm as the Python library, so a Rust and a Python
worker compute the same hash for the same machine. Restore refuses a snapshot whose hash does not
match the machine, which is how a deployed machine and a stored snapshot detect that they have
drifted.

The layout is byte-compatible with the Python library (ADR-0003): Rust writes compact JSON where
Python indents by 2, and a conformance test pins the two texts equal once whitespace is removed.
