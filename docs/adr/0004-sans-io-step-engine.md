# ADR-0004: Sans-I/O step engine shared by the sync and async interpreters

- **Status:** accepted (Phase 0, 2026-11-01)

## Context

Python shares one `async def` algorithm between its sync and asyncio engines by driving the
coroutine with a single `coro.send(None)` and requiring that it never suspends. Rust has no
equivalent trick without duplicating the algorithm or forcing an async runtime on sync users.

## Decision

`xstate-core` exposes a pure step function:

```text
fn step(machine, &mut ActiveState<C>, event, logic, &mut Effects) -> Result<StepReport>
```

It performs selection, exit/transition/entry, eventless settling and `onDone` propagation, and
appends side effects to `Effects` instead of performing them: `ArmTimer`, `CancelTimers`,
`StartInvoke`, `StopActor`, `Enqueue(event, lane)`, `Emit`, `SendTo`, `Log`.

- `SyncInterpreter` (in `xstate-core`) executes effects inline with a timer heap pumped by `send`
  and `tick`.
- `Interpreter` (in `xstate-tokio`) maps effects onto tasks, channels and `tokio::time`.
- The pure API (`initial_transition`, `transition`) is the step function with effects discarded.
- Graph utilities, the CLI simulator and the conformance harness drive the step function
  directly with a `SimulatedClock`.

## Consequences

- One algorithm, one test surface; the core crate has no I/O and compiles to `wasm32`.
- Services/actors are an `ActorLogic` enum/trait implemented by the drivers; the core only knows
  "start invoke X with input Y" and "deliver done/error event".
