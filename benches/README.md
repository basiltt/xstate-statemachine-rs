# Benchmarks and the performance gate

Benchmarks live next to the crates they measure (`crates/*/benches/`) and run with
`cargo bench --workspace`. They use [criterion](https://github.com/bheisler/criterion.rs).

`budgets.json` in this directory holds the reference p50 for each benchmark id together with
an allowed slowdown factor. It plays the role of the Python library's
`benchmarks/budgets.json` + `perf_gate.py`: the gate is *relative* (a machine speed factor is
estimated from the median ratio across all rows, then every row must stay within its budget
after that adjustment), so it works on noisy CI runners.

Phase 1 adds the gate runner (`cargo xtask perf-gate`) and the engine benchmarks:

| id | what it measures | Python reference (p50) | Rust target |
|---|---|---|---|
| `send_flat` | one event on a 2-state machine, no actions | 14 µs | ≤ 200 ns |
| `send_guard_assign` | guard + `assign` on a dynamic context | 16 µs | ≤ 600 ns |
| `send_nested_3` | 3-level hierarchy, cross-branch transition | 35 µs | ≤ 500 ns |
| `send_parallel_2` | two orthogonal regions toggling | 39 µs | ≤ 500 ns |
| `build_50_states` | parse + validate + compile 50-state JSON | 115 µs | ≤ 40 µs |
| `snapshot_get` | serialize snapshot of a 50-state machine | 12 µs | ≤ 3 µs |
| `snapshot_restore` | parse + validate + restore | 23 µs | ≤ 5 µs |
| `persisted_memory` | load → send → save round trip, memory store | 87 µs | ≤ 8 µs |

Targets are laptop-core estimates from the feasibility report §6; the first measured run
replaces them with real baselines.
