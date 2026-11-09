# §6.2 Battle test — xstate-macros, xstate-inspect, and xstate-observability

Scope: the §6.2 `xstate-macros`, `xstate-inspect`, and `xstate-observability` matrices in `docs/HANDOVER-QA.md`:
- `xstate-macros`: compile-fail error reporting for all validation rules, `#![deny(warnings)]`
  and `clippy::pedantic` compliance on generated code, facade-only crate dependencies, renamed
  dependency imports, naming convention conversions (`snake_case`, `kebab-case`, `PascalCase`),
  and large hierarchical statecharts (60 nesting levels deep, 200 sibling states wide).
- `xstate-inspect`: extended golden protocol tests (`after` delayed transitions, invoke completion
  and error events, nested payload redaction with `***`), `SseSink` connection limit enforcement (128
  concurrent connections), and deterministic LCG fuzzing with random byte streams.
- `xstate-observability`: metric family name and label key equivalence with the Python
  `PrometheusPlugin` 0.12.1 scrape output, cardinality protection under a flood of 10,000 undeclared
  events, and duration recording with `timed`.

Severity counts: **0 CRITICAL · 0 HIGH · 0 MEDIUM · 0 LOW** (all invariants verified and sound).

New test files:
- `crates/xstate-macros/tests/large_chart.rs` — 60-level deep and 200-sibling wide charts.
- `crates/xstate-macros/tests/lints.rs` and `lints/` — pedantic clippy clean compilation of generated code.
- `crates/xstate-macros/tests/rename_all.rs` — variant casing transforms.
- `crates/xstate-macros/tests/renamed.rs` and `renamed/` — usage when `xstate` crate is aliased.
- `crates/xstate-macros/tests/root_keys.rs` — root-level metadata and target relaxation.
- `crates/xstate-macros/tests/ui/*.rs` and `*.stderr` — 27 trybuild UI compile-fail diagnostic tests.
- `crates/xstate-inspect/tests/golden_extended.rs` — invoke lifecycle and delayed transition inspect events.
- `crates/xstate-inspect/tests/sse_fuzz.rs` — deterministic fuzzing and 128-connection cap enforcement.
- `crates/xstate-observability/tests/metric_parity.rs` — scrape-time metric names, labels, and cardinality limits.

## Verified sound

### xstate-macros
- **Compile-time validation**: comprehensive coverage of invalid charts (missing initial states,
  unresolved targets, duplicate keys, unknown keywords, duplicate delays, missing ids) producing
  clear compiler diagnostics and spans via trybuild.
- **Hygiene & lints**: macro expansions generate clean code passing `clippy::pedantic` and
  `#![deny(warnings)]`.
- **Scaling**: deep recursion (60 levels) and wide sibling states (200 states) compile and execute
  without hitting compiler recursion limits or stack exhaustion.

### xstate-inspect
- **Protocol conformance**: events match Stately Inspector specifications; delayed transitions emit
  standard `after.*` events, and child services emit `done.invoke` and `error.platform`.
- **Secret redaction**: nested sensitive values in allow-listed event payloads are masked with `***`.
- **SSE server resilience**: random byte fuzzing (300 iterations) never crashes the server or leaks
  internal panics. Connection cap strictly limits live connections to 128 and cleanly drops excess clients.

### xstate-observability
- **Python Prometheus parity**: counter, gauge, and histogram metric names and labels match the Python
  0.12.1 implementation scrape format.
- **Cardinality hygiene**: undeclared dynamic events collapse into the `"unknown"` category rather than
  minting unbounded time-series metrics.
