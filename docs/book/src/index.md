# xstate-statemachine for Rust

`xstate-statemachine` is a Rust port of the Python
[`xstate-statemachine`](https://pypi.org/project/xstate-statemachine/) library: a statechart
runtime compatible with the real [XState v5](https://stately.ai/docs/xstate) engine, built for
speed (engine dispatch benchmarks are in the tens-to-hundreds-of-nanoseconds per event) with full
type safety, byte-compatible snapshots and store layouts with the Python library, and an
event-driven-architecture toolset (CloudEvents envelopes, brokers, transactional outbox, dead
letters, AsyncAPI), persistence with durable timers and idempotency, resilience patterns,
observability, the Stately Inspector protocol, HTTP integration, LLM-agent charts and a CLI.

> **Status: pre-release.** Workspace version 0.1.0, unpublished. Treat performance numbers and
> parity claims as machine- and version-specific; the QA pass that exercised them is tracked in
> the repository (`docs/qa/REPORT.md`), and the public API is the same code rustdoc renders
> under [`/api/`](https://basiltt.github.io/xstate-statemachine-rs/api/xstate_core/) once the site is published.

## Find your way

- [Getting started](guide/getting-started.md) — install, run a chart, the Stately editor export.
- [Concepts](concepts.md) — the semantics the engine implements, in short pages.
- [Guides](guide/tokio.md) — one per feature: persistence, EDA, patterns, inspector, axum, agents,
  macros, Python bindings, CLI.
- [Interoperability with Python](guide/migrating-from-python.md) — what is byte-compatible and how
  to share stores and brokers.
- [Reference](guide/features.md) — feature flags, environment variables, errors, MSRV/SemVer, ADRs.
- [Operations](operations/deployment.md) — deployment, security hardening, monitoring, backup.

The engine follows the XState v5 algorithm (as realised by the W3C SCXML Appendix D semantics).
Wherever the Python library and XState v5 disagree, this library follows XState v5 (ADR-0001); the
conformance suite pins that with 53 cases checked against the real XState engine and the Python
library. See the [design decisions](reference/adr.md) and the
[changelog](changelog.md) for the rest.
