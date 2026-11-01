# ADR-0002: Two-tier context and event model

- **Status:** accepted (Phase 0, 2026-11-01)
- **Related:** [feasibility report §5.2](../feasibility-report.md)

## Context

Python's context is a mutable `dict` mutated in place by actions; events are duck-typed objects
with `.type` and `.payload`. XState JSON (`assign`, `input`, `output`, `params`) is written in
terms of JSON values. Rust users expect typed contexts and events, but a JSON-driven engine (the
Stately workflow, shared stores with Python, the CLI simulator) needs a dynamic representation.

## Decision

1. **Dynamic tier (default).** `DynContext = serde_json::Value` and
   `Event { ty: EventType, payload: serde_json::Value }`. This is what JSON machines, the CLI,
   persistence and the Python-compatible snapshot format use. JSON-form `assign` operates on it.
2. **Typed tier.** The engine is generic over `C: Serialize + DeserializeOwned + Clone + Send +
   Sync + 'static` (context) and over an event type `E: Serialize + DeserializeOwned + Send +
   'static` that implements `EventType` (returns its XState type string). Actions receive
   `&mut C`; guards receive `&C`. The `statechart!` DSL (Phase 1b) generates typed enums.
3. **Rollback** (`actionErrorPolicy = "rollback"`) requires `C: Clone`. Documentation recommends
   cheap-to-clone contexts (`Arc`, persistent collections) for large states.
4. **Logic is registered by name** in `MachineLogic<C, E>` (actions, guards, services, delays).
   Missing implementations are detected when the machine is built, not at run time.
5. **No callables inside configuration.** Property-level `assign` functions, `input`/`output`
   factories, computed delays and callable `sendTo` targets exist only as registered logic, never
   as config values. JSON stays pure data, as in XState v5.

## Consequences

- Dynamic-tier action cost is dominated by `serde_json::Value` manipulation; the typed tier is
  the performance path and is benchmarked separately.
- Snapshot `context` round-trips through serde in both tiers, so persistence is tier-agnostic.
