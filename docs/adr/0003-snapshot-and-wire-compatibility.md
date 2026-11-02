# ADR-0003: Byte-compatible snapshots, store schemas and wire formats with the Python library

- **Status:** accepted (Phase 0, 2026-11-01)
- **Related:** ADR-0001, [feasibility report §4.3](../feasibility-report.md)

## Context

The Python library persists interpreter state as snapshot layout v4 (`version`, `machine_id`,
`machine_hash`, `machine_version`, `taken_at`, `status`, `context`, `state_ids`, `value`,
`configuration`, `history`, `output`, `error`, `chain_trips`, `last_chain_error`, `deferred`,
`pending_events`, `scheduled_sends`, `deadlines`, `actors`, `system`) in stores with fixed
table/key layouts (SQLite `statecharts`/`deadlines`, Redis `snap:{key}` hashes, SQL
`xsm_*` tables). It also speaks fixed wire formats: CloudEvents envelopes, the Stately Inspector
protocol pinned to `@statelyai/inspect` 0.7.2, AsyncAPI 3.0 documents.

A Rust worker that can resume a Python-created instance (and vice versa) is the strongest
migration story and the main differentiator over a greenfield Rust statechart crate.

## Decision

1. The Rust snapshot type is **layout v4, compatible with Python's**: the same fields in the same
   order with the same values, so that `Snapshot::to_json()` equals the text of Python's
   `get_snapshot()` once the insignificant whitespace is removed (Python indents by 2, Rust writes
   compact JSON). The structure hash is byte-identical. Compatibility includes:
   - `machine_hash`: first 16 hex chars of SHA-256 over the canonical, sorted-key JSON of the
     per-node structural shape, exactly as `persistence/snapshot.py::structure_hash` defines it.
   - `state_ids` sorted, `configuration` sorted, `taken_at` as a float epoch, status strings.
2. Store schemas (SQLite, Redis, SQL) reuse the Python table and key layouts and schema version
   markers so a single database can serve both runtimes.
3. Envelope, inspector protocol and AsyncAPI output are byte-compatible as well.
4. Python-generated fixtures (`conformance/cases/*.json` → `expected.python.snapshot`,
   `expected.python.snapshot_text`, `expected.python.machine_hash`) are the test vectors; the Rust
   `machine_hash` must reproduce them exactly, the snapshot must equal `snapshot` as a value and
   `snapshot_text` as text (whitespace removed, volatile wall-clock fields zeroed on both sides), and
   the Python text must restore through `from_snapshot`.
5. Event-name aliasing (ADR-0001) is applied when *restoring* a snapshot whose queued events use
   v4 names, so old snapshots stay loadable.

## Consequences

- JSON float/string formatting must match Python's `json.dumps(sort_keys=True)` for the hash
  input. A dedicated canonical-JSON writer is part of `xstate-core`.
- Any future layout change (v5) is coordinated across both libraries.
