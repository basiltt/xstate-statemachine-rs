# Invoke and actors

`invoke` starts an actor when a state is entered. `MachineLogic` registers services:

- `service(name, |ctx, input| -> Value)` — a synchronous service, run to completion; its result is
  the `xstate.done.actor.<id>` output, an error becomes `xstate.error.actor.<id>`.
- `async_service(name, |ctx, input| -> future)` — runs on async interpreters.
- `machine_service(name, machine)` — a child chart as an actor.

`onDone` / `onError` transition on the done / error events; `id` and `systemId` address the actor
for `sendTo` / `sendParent`. In memory the engine emits the v5 event spellings
(`xstate.done.actor.*`, `xstate.error.actor.*`) and accepts the v4 spellings on input; snapshots
and the structure hash use the v4 spellings (ADR-0001, ADR-0003).

The async runtime (tokio) runs child machines as tasks and supports `spawnChild` / `stopChild`.
The core engine only knows "start invoke X with input Y" and "deliver the done/error event" — the
driver decides how to run it (ADR-0004).
