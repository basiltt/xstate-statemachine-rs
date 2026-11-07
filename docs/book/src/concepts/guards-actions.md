# Guards and actions

JSON charts name their logic; Rust supplies it in a `MachineLogic`:

- **Guards** — `Fn(&C, &Event, &Value) -> Result<bool>`: the context, the event and the guard's
  static `params`. Composite guards (`and`, `or`, `not`, `stateIn`) are evaluated by the engine.
- **Actions** — `Fn(&mut ActionScope<C>) -> Result<()>`. `ActionScope` exposes `context`
  (`&mut C`), `event`, `params`, and the effect helpers `raise(event)` and `emit(value)`.

## Built-in actions

`assign`, `raise`, `sendTo`, `sendParent`, `forwardTo`, `escalate`, `log`, `cancel`, `stopChild`,
`spawnChild`, `emit` and `choose` are built in. A JSON `assign` operates on the dynamic context;
anything that must compute values from the event is a *named* action writing `scope.context`.
The callback-style built-ins `enqueueActions` and `pure` are not supported: move them into named
logic.

## Failure is explicit

A guard that returns `Err`, or a missing implementation, is an error, not a silent `false`. A name
the chart uses but the registry lacks is reported by `start()` as `ImplementationMissing` before
any event runs. An action failure follows `actionErrorPolicy` (see
[policies](policies.md)).
