# xstate-statemachine (Rust)

**Statecharts for Rust, compatible with [XState v5](https://stately.ai/docs/xstate) and the
[Stately editor](https://stately.ai/editor).** Design the chart visually, export it as JSON,
supply the named guards, actions and services in Rust, and run it — hierarchy, parallel regions,
history, delayed transitions, invoked services, actors, snapshots and durable persistence.

This is the Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine). Same JSON dialect, same
snapshot format and store layouts (a Rust worker can resume an instance a Python worker saved),
same CLI; built for speed with Rust's type system doing the checking.

> **Status: Phases 1–4 landed — engine, persistence, observability, inspector, CLI and EDA.**
> `xstate-core` parses XState v5 JSON, compiles it, and runs it with a synchronous
> interpreter; `xstate-tokio` runs it as an async actor with timers, async services and child
> actors; `xstate-persistence` saves and resumes instances in Python-compatible stores;
> `xstate-inspect` streams Stately Inspector messages and `xstate-observability` records
> metrics; `xsm` is the command-line tool; `xstate-eda` and `xstate-patterns` bring CloudEvents
> envelopes, the inbound dispatcher, outbox, dead letters, AsyncAPI, retry, circuit breaker,
> saga and choreography. The engine matches XState v5 on every conformance case and produces
> Python-identical snapshots, structure hashes, inspector recordings, saga charts and AsyncAPI
> documents. Remaining integrations (Redis/sqlx stores, broker adapters, typed DSL macros,
> axum, agents, Python bindings) follow. See [`docs/feasibility-report.md`](docs/feasibility-report.md) and
> [`docs/adr/`](docs/adr/).

## Quick start

```rust
use xstate_core::{Machine, MachineLogic, SyncInterpreter};

let machine = Machine::from_json(r#"{
  "id": "door", "initial": "closed", "context": {"opens": 0},
  "states": {
    "closed": {"on": {"OPEN": {"target": "opened", "guard": "isUnlocked", "actions": "countOpen"}}},
    "opened": {"on": {"CLOSE": "closed"}}
  }
}"#)?;
let logic = MachineLogic::dynamic()
    .guard("isUnlocked", |ctx, _event, _params| Ok(ctx["locked"] != true))
    .action("countOpen", |scope| {
        let n = scope.context["opens"].as_u64().unwrap_or(0) + 1;
        scope.context["opens"] = n.into();
        Ok(())
    });
let mut door = SyncInterpreter::new(machine, logic);
door.start()?;
door.send("OPEN")?;
assert_eq!(door.state_ids(), ["door.opened"]);
# Ok::<(), xstate_core::Error>(())
```

## Workspace

| Crate | Purpose |
|---|---|
| [`xstate-core`](crates/xstate-core) | machine model, JSON parser/validator, sans-I/O step engine, sync interpreter, snapshots, pure API |
| [`xstate-tokio`](crates/xstate-tokio) | async interpreter on tokio: mailbox, timers, invoked services, actors |
| [`xstate-persistence`](crates/xstate-persistence) | stores, locks, `persisted` wrapper, durable deadlines, idempotency inbox, audit log |
| [`xstate-testing`](crates/xstate-testing) | stub logic, simulated clock fixtures, path walking, coverage |
| [`xstate-conformance`](crates/xstate-conformance) | fixture model and harness for the conformance suite (dev only) |
| [`xstate-inspect`](crates/xstate-inspect) | Stately Inspector protocol: observer, allow-lists and redaction, JSONL recording and replay |
| [`xstate-observability`](crates/xstate-observability) | `metrics`-facade observer with the Python `PrometheusPlugin` metric set and label hygiene |
| [`xstate-eda`](crates/xstate-eda) | CloudEvents envelopes, broker contract and fake broker, inbound dispatcher, transactional outbox, dead letters, AsyncAPI 3.0 |
| [`xstate-patterns`](crates/xstate-patterns) | retry policy, circuit breaker, saga builder, choreography router |
| [`xsm`](crates/xsm) | command-line tool: validate, inspect, diagram, docs, simulate, paths, snapshots |

Planned for later phases: broker adapters (Kafka, RabbitMQ, NATS, SQS, Redis Streams),
`xstate-macros` (typed DSL), `xstate-axum`, `xstate-agents`, `xstate-py`.

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
