# Concepts

This section explains the semantics the engine implements, in short pages. For the full type
signatures see the rustdoc under `/api/xstate_core/` once the site is published.

Two design decisions shape everything (see the [ADRs](reference/adr.md)):

- **XState v5 is the conformance oracle** (ADR-0001). Where the Python library and XState v5
  disagree, this library follows XState v5, because the Stately editor's simulator *is* XState.
- **The engine is sans-I/O** (ADR-0004). `xstate-core` exposes a pure step function that computes
  the next state and appends side effects — arm/cancel timers, start/stop actors, raise, emit — to
  an `Effects` buffer instead of performing them. `SyncInterpreter` executes those effects with a
  timer heap; the tokio `Interpreter` maps them onto tasks, channels and `tokio::time`. The core
  crate has no I/O, no async runtime and no required features.

A machine is *data*: an XState v5 JSON document. Its guards, actions, services and delays are Rust
functions you register by name in a `MachineLogic`. The pages below cover the pieces:

- [Events and context](concepts/events-context.md)
- [Guards and actions](concepts/guards-actions.md)
- [Delays (`after`)](concepts/delays.md)
- [Invoke and actors](concepts/invoke-actors.md)
- [Snapshots and the structure hash](concepts/snapshots.md)
- [Policies](concepts/policies.md)
