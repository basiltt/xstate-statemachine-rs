# Conformance fixtures

Executable specification of the engine. Each file in `cases/` is one scenario: a machine in
XState JSON, stubbed logic, a sequence of steps, and the behaviour two independent oracles
observed. The Rust engine is correct when it reproduces the `expected.xstate` observations
(ADR-0001) and the Python `machine_hash` / snapshot vectors (ADR-0003).

## Oracles

| Oracle | What it provides | Tool |
|---|---|---|
| **xstate** (XState v5 under Node, the engine behind the Stately editor's simulator) | `initial` and per-step observations — the semantic reference | `tools/xstate_oracle/oracle.mjs` |
| **python** (`xstate-statemachine`, the library being ported) | the same observations for comparison, plus `machine_hash` and the end-of-run `snapshot` (layout v4) | `tools/generate_expected.py` |

When the two oracles disagree the case must carry a `divergence` note; the current list is in
[`divergences.generated.md`](divergences.generated.md).

## Case format (`schema/case.schema.json`)

```jsonc
{
  "name": "toggle_basic",                 // unique, snake_case
  "description": "...",
  "tags": ["flat"],
  "machine": { /* XState v5 JSON, as exported by the Stately editor */ },
  "logic": {                               // optional stubs; everything defaults to a no-op
    "guards":   { "isUnlocked": false },   // constant results; unlisted guards are true
    "services": { "fetchUser": { "result": {"name": "Ada"} } }, // or { "error": "boom" }
    "delays":   { "BACKOFF": 1000 }        // named delays in ms
  },
  "steps": [
    { "send": "TOGGLE" },                  // event type only
    { "send": { "type": "ADD", "qty": 2 } }, // event with payload fields
    { "advance_ms": 500 }                  // move the simulated clock
  ],
  "divergence": {                          // only when the oracles disagree
    "python": "what Python does", "spec": "what SCXML/XState does", "rust_follows": "xstate"
  },
  "expected": { "xstate": Expectation, "python": Expectation }   // generated, never hand-edited
}
```

An **observation** is taken after `start()` (`initial`) and after every step:

```jsonc
{
  "state_ids": ["toggle.active"],  // active leaf state ids, sorted; format "<machine id>.<path>"
  "context": { },                  // context after the step
  "actions": ["countOpen"],        // user actions executed since the previous observation, in order
  "status": "running",             // running | done | error | stopped
  "output": null,                  // present when status is done
  "error": "boom"                  // present when status is error
}
```

Built-in actions (`assign`, `raise`, `log`, `sendTo`, ...) are not recorded; only named user
actions are. The Python expectation additionally carries `machine_hash` and `snapshot`, in which
the volatile fields `taken_at` and `deadlines[].due_at_wall` are zeroed.

## JSON dialect notes

- `"E": null` (forbidden transition, Python/Rust dialect) is translated to XState's empty
  targetless transition `{}` before running the xstate oracle; both mean "consume the event and
  block ancestors".
- Built-in actions are written in JSON form, `{"type": "assign", "params": {"assignment": {...}}}`
  and `{"type": "raise", "params": {"event": {...}}}`; the xstate oracle provides matching
  implementations under those names.

## Regenerating

```bash
npm ci --prefix conformance/tools/xstate_oracle
python conformance/tools/generate_expected.py          # rewrite
python conformance/tools/generate_expected.py --check  # CI: fail when stale or undocumented divergence
```

Add a case by writing the file without `expected` and running the generator.

## Rust side

`crates/xstate-conformance` holds the typed fixture model (`Case`, `Observation`, ...) and a
test that loads every file. Phase 1 adds the runner that drives the engine through each case and
asserts against `expected.xstate`, plus the hash/snapshot assertions against `expected.python`.
