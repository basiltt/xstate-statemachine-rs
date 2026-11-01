# ADR-0001: Engine semantics follow SCXML / XState v5, not the Python engine's divergences

- **Status:** accepted (Phase 0, 2026-11-01)
- **Deciders:** Basil T T
- **Related:** [feasibility report §4.2](../feasibility-report.md), ADR-0003

## Context

The Python engine (`xstate-statemachine` 0.12.0) implements the SCXML algorithm but, as the
source inventory showed, diverges from the W3C SCXML Appendix D algorithm and from XState v5 in
a handful of undocumented places:

| Python behaviour | Spec / XState v5 |
|---|---|
| `done.state.*` is queued on the **external** queue | internal queue |
| Transitions selected in one microstep run sequentially, with no conflict preemption | optimal enabled transition set with preemption; grouped exit → actions → entry |
| Exit-order ties broken by state-id string | reverse document order |
| Every candidate guard is evaluated (memoised) | evaluation stops once a transition is selected for a leaf |
| `assign` applied in place; later property functions see earlier keys' writes | the whole assignment is computed against the pre-action context |
| `reenter` ignored for non-self targets | `reenter` controls whether a descendant target exits the source |
| Parallel entry: the targeted region is entered last | document order |
| v4 event names (`done.invoke.*`, `error.platform.*`) | v5 names (`xstate.done.actor.*`, `xstate.error.actor.*`) |

The headline promise of the port is "design the chart in the Stately editor, run it unchanged,
behave exactly as the editor's simulator (the real XState v5 engine) does".

## Decision

1. The Rust engine implements the **W3C SCXML Appendix D algorithm as realised by XState v5**.
   Where SCXML and XState differ, XState v5 wins, because Stately's simulator is XState.
2. **XState v5 itself is the conformance oracle.** `conformance/tools/xstate_oracle` runs every
   fixture through the real `xstate` package under Node; the Rust engine must reproduce its
   state values, context and user-action order. The Python engine is a *secondary* oracle, used
   for snapshot/hash/wire-format vectors (ADR-0003), not for transition semantics.
3. Fixtures where the two oracles disagree are recorded with a `divergence` note. They document
   the Python bugs to fix upstream; the Rust engine follows XState.
4. Event names: the engine emits the **v5 spellings** and accepts the v4 spellings on input
   (`on: {"done.invoke.x": ...}`) as aliases, so existing Python-authored charts keep working.
   Snapshots store v5 names; the snapshot shape stays layout-compatible (ADR-0003).
5. No `engine_compat = "python"` switch is built unless a concrete need appears.

## Consequences

- A small number of Python parity tests encode Python-specific behaviour and will not port
  unchanged; they are tracked in the fixture `divergence` fields.
- The Python library should receive coordinated fixes for the items above in a minor release so
  that both engines converge; until then, shared stores work (snapshots are state, not
  semantics) but a mixed fleet may order some internal events differently.
