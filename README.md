# xstate-statemachine (Rust)

**Statecharts for Rust, compatible with [XState v5](https://stately.ai/docs/xstate) and the
[Stately editor](https://stately.ai/editor).** Design the chart visually, export it as JSON,
supply the named guards, actions and services in Rust, and run it — hierarchy, parallel regions,
history, delayed transitions, invoked services, actors, snapshots and durable persistence.

This is the Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine). Same JSON dialect, same
snapshot format and store layouts (a Rust worker can resume an instance a Python worker saved),
same CLI; built for speed with Rust's type system doing the checking.

> **Status: Phase 0 — design and skeleton.** The engine lands in Phase 1. See
> [`docs/feasibility-report.md`](docs/feasibility-report.md) for the study and
> [`docs/adr/`](docs/adr/) for the decisions taken.

## Workspace

| Crate | Purpose |
|---|---|
| [`xstate-core`](crates/xstate-core) | machine model, JSON parser/validator, sans-I/O step engine, sync interpreter, snapshots, pure API |
| [`xstate-tokio`](crates/xstate-tokio) | async interpreter on tokio: mailbox, timers, invoked services, actors |
| [`xstate-persistence`](crates/xstate-persistence) | stores, locks, `persisted` wrapper, durable deadlines, idempotency inbox, audit log |
| [`xstate-testing`](crates/xstate-testing) | stub logic, simulated clock fixtures, path walking, coverage |
| [`xstate-conformance`](crates/xstate-conformance) | fixture model and harness for the conformance suite (dev only) |
| [`xsm`](crates/xsm) | command-line tool |

Planned for later phases: `xstate-macros` (typed DSL), `xstate-eda`, `xstate-patterns`,
`xstate-observability`, `xstate-inspect`, `xstate-axum`, `xstate-agents`, `xstate-py`.

## Conformance

The engine is specified by executable fixtures in [`conformance/`](conformance/README.md). Each
case is run through the real XState v5 engine (Node) and through the Python library; the Rust
engine must match XState's observations and reproduce Python's snapshot bytes and structure hash.
Disagreements between the two oracles are listed in
[`conformance/divergences.generated.md`](conformance/divergences.generated.md).

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
cargo fmt --all --check
cargo deny check          # cargo install cargo-deny
```

Regenerate conformance expectations (needs Node ≥ 20 and the Python library):

```bash
npm ci --prefix conformance/tools/xstate_oracle
python conformance/tools/generate_expected.py
```

## License

MIT — see [LICENSE](LICENSE).
