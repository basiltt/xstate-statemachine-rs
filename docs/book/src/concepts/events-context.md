# Events and context

Two tiers serve two kinds of user (ADR-0002).

## Dynamic tier (default)

The tier JSON machines, the CLI and persistence use:

- **Context** is `serde_json::Value` (`DynContext`).
- **Events** are `Event { ty, payload }` with a JSON payload.

`Machine::from_json` produces a machine over this tier, and `MachineLogic::dynamic()` registers
the named logic. Actions receive `&mut` context and mutate it in place; guards receive `&` context.

## Typed tier

The engine is generic over the context and event types:

- context `C: Serialize + DeserializeOwned + Clone + Send + Sync + 'static`
- event `E: Serialize + DeserializeOwned + Send + 'static` implementing `EventType`

The `statechart!` DSL and `#[derive(TypedEvent)]` / `#[derive(TypedState)]` (feature `macros`)
generate typed enums for this tier, so the compiler checks that every event and state name is real.

## Rollback needs a cloneable context

`actionErrorPolicy: "rollback"` restores the context, which requires `C: Clone`. Cheap-to-clone
contexts (`Arc`, persistent collections) are recommended for large states.

## Where the two tiers meet

Snapshots serialise context through serde in both tiers, so persistence and the Python
compatibility story are tier-agnostic.
