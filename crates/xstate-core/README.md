# xstate-core

Core engine of **xstate-statemachine** for Rust: an XState v5 / W3C SCXML compatible statechart
runtime. Load a machine exported from the [Stately editor](https://stately.ai/editor) as JSON,
supply the named guards, actions and services in Rust, and run it.

Status: Phase 1 — parser, compiler, sans-I/O step engine, synchronous interpreter, snapshots,
pure API, builder, exporters and graph utilities. See `docs/adr/` in the repository for the design.
