# Stately editor to Rust

The promise of this library: **draw the chart in the [Stately editor](https://stately.ai/editor),
export it as JSON, supply the named logic in Rust, and run it**. The engine follows XState v5
semantics (ADR-0001), so the chart behaves as it does in the editor's simulator.

## The workflow

1. **Design** the chart in the editor. Give every guard, action, service and delay a name; the
   JSON carries the names, not code.
2. **Export** it: in the editor choose *Export* and *JSON*, and save the file (for example
   `checkout.json`). The keys the editor adds for TypeScript tooling (`types`, `schemas`,
   `$schema`) are accepted and ignored.
3. **Check** it from the terminal before writing any Rust:

   ```bash
   xsm validate checkout.json     # builds it with the real engine and reports every finding
   xsm inspect checkout.json      # state tree, transitions, and the logic names to implement
   xsm simulate checkout.json -e ADD,PAY,+30000   # run it on a simulated clock, stub logic
   xsm diagram checkout.json -f mermaid           # for the README or a review
   ```

4. **Implement** the names it lists and **run** it.

## Running an export

This is a typical editor export (`types` is editor metadata; `after` carries a delay in
milliseconds; `guard` and `actions` are names):

```json
{
  "id": "checkout",
  "initial": "cart",
  "context": {"items": 0},
  "types": {"events": {}},
  "states": {
    "cart": {"on": {"ADD": {"actions": "addItem"},
                    "PAY": {"target": "paying", "guard": "hasItems"}}},
    "paying": {
      "after": {"30000": "expired"},
      "on": {"PAID": "done", "FAILED": "cart"}
    },
    "expired": {"always": {"target": "cart"}},
    "done": {"type": "final"}
  }
}
```

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::SimulatedClock;

let machine = Machine::from_json(&std::fs::read_to_string("checkout.json")?)?;
for warning in machine.warnings() {
    eprintln!("{}: {}", warning.path, warning.message); // unknown keys and the like
}

let logic = MachineLogic::dynamic()
    .guard("hasItems", |ctx, _event, _params| Ok(ctx["items"].as_u64().unwrap_or(0) > 0))
    .action("addItem", |scope| {
        let n = scope.context["items"].as_u64().unwrap_or(0) + 1;
        scope.context["items"] = n.into();
        Ok(())
    });

let clock = Arc::new(SimulatedClock::new());           // RealClock::new() in production
let mut checkout = SyncInterpreter::with_clock(machine, logic, clock.clone());
checkout.start()?;
checkout.send("PAY")?;                                 // guard is false: stays in the cart
checkout.send("ADD")?;
checkout.send("PAY")?;
assert_eq!(checkout.state_ids(), ["checkout.paying"]);
checkout.advance(&clock, 31_000.0)?;                   // the 30 s timeout fires
assert_eq!(checkout.state_ids(), ["checkout.cart"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

For an async runtime use `spawn` from the `tokio` feature with the same `Machine` and logic;
services named by `invoke.src` are registered with `async_service` (a future) or `service` (a
blocking function), and a child chart with `machine_service`.

## What is supported

The parser accepts the XState v5 JSON dialect, plus the v4 spellings Python-authored charts use
(`cond`, `internal`, `done.invoke.*`).

| Area | Supported |
|---|---|
| States | atomic, compound, parallel, final, shallow and deep history, `initial` (inferred when omitted), custom `#id`s, `tags`, `meta`, `description` |
| Transitions | `on` (exact, `*`, `foo.*`), `always`, `after` (milliseconds or a named delay), `onDone`, `reenter`, forbidden transitions (`null` / `{}`), lists with first-match, shorthand `"E": "target"`, targets by `#id`, `.child`, sibling and multi-target |
| Guards | named, `{type, params}`, composite `and` / `or` / `not`, `stateIn` |
| Actions | named user actions with `params`; built-ins `assign` (JSON values), `raise`, `sendTo`, `sendParent`, `forwardTo`, `escalate`, `log`, `cancel`, `stopChild`, `spawnChild`, `emit`, `choose` |
| Invoke | `src` by name (sync, async, child machine), `input`, `systemId`, `onDone`, `onError`, done / error events (`xstate.done.actor.*`, `xstate.error.actor.*`) |
| Root keys | `context`, `version`, `output`, and the policies `strict`, `strictTargets`, `strictConfig`, `maxIterations`, `actionErrorPolicy`, `guardErrorPolicy`, `onUnhandled` |

Machines also round-trip: `machine.to_json()`, `to_mermaid()` and `to_plantuml()` (the last two
match the Python output byte for byte on the fixture machines).

## What is different from the editor's JavaScript

The editor exports data; JavaScript functions never appear in it, and neither can they here
(ADR-0002). That has three consequences.

- **Names are implemented in Rust.** An `assign` whose values are computed from the event is a
  named action that writes `scope.context`; static `assign` objects in the JSON work as they are.
  Guards receive `(&context, &event, &params)`; actions receive an `ActionScope` with the context,
  the event, the `params` and `raise` / `emit`.
- **Missing names fail early.** `start()` returns `ImplementationMissing` for the first guard,
  action, service or delay the chart uses and the registry lacks.
- **Not supported:** the callback-style built-ins `enqueueActions` and `pure`, and any config
  value that is a function in the JavaScript source (computed `input`, `output` factories,
  function delays): move them into named logic. Unknown keys are warnings by default; set
  `"strictConfig": true` to make them errors.

Where the Python library and XState v5 disagree, the Rust engine follows XState; the list is in
[`conformance/divergences.generated.md`](../../conformance/divergences.generated.md) and
summarised in [migrating-from-python.md](migrating-from-python.md).

## Typed alternative

If you would rather author in Rust, `statechart!` (feature `macros`) and the
`MachineBuilder` in `xstate_core::builder` produce the same `Machine`, and `machine.to_json()`
gives the JSON to open in the editor.
