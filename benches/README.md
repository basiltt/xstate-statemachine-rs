# Benchmarks and the performance gate

Benchmarks live next to the crates they measure (`crates/*/benches/`) and run with
`cargo bench --workspace`. They use [criterion](https://github.com/bheisler/criterion.rs).

`budgets.json` in this directory holds, per benchmark id, the reference median (`baseline_p50_ns`),
an optional target, and for the Python-parity scenarios the Python library's time for the same
operation (`python_per_op_ns`). [`scripts/perf_gate.py`](../scripts/perf_gate.py) (Python
standard library only) runs the engine benchmarks, reads criterion's
`target/criterion/<id>/new/estimates.json`, and compares each median against
`baseline_p50_ns × allowed_slowdown` (`allowed_slowdown` = 1.25):

```sh
python scripts/perf_gate.py                      # run core + python_parity, fail (exit 1) on a regression
python scripts/perf_gate.py --no-run             # gate the estimates of the last `cargo bench` run
python scripts/perf_gate.py --relative           # scale limits by the machine speed factor (see below)
python scripts/perf_gate.py --advisory           # print the table, never fail
python scripts/perf_gate.py --update-baselines   # run, then record the medians as the new baselines
python scripts/perf_gate.py -- --warm-up-time 1 --measurement-time 3   # extra criterion arguments
```

The table lists the median, the baseline and limit, the Python time per operation, the speed-up
over Python, and how far the scenario is from 100x.

`--relative` divides out the speed of the machine: the median of `measured / baseline` over every
measured row is taken as the machine speed factor and every limit is multiplied by it, so a
uniformly slower (or busier) machine passes while one benchmark regressing against the rest still
fails. **CI runs the gate with `--advisory --relative`**: hosted runners differ from the machine
the baselines were recorded on and from run to run, so the CI job prints the table but never
fails the build. Gate for real on the reference machine, quiet (no build, browser or other
CPU-heavy process running), with the default absolute mode, and refresh the baselines with
`--update-baselines` after an intended change (run it twice and keep the better run).

## The engine benchmarks

`crates/xstate-core/benches/python_parity.rs` mirrors scenarios S1 to S7 of the Python library's
cross-library benchmark (`benchmarks/competitors/adapters/xstate_sync.py` of xstate-statemachine
0.12.0): the same machines, the same logic (a `DynContext` JSON context; S2's guard and action are
Rust closures registered on `MachineLogic`, the equivalent of Python's callables) and the same work
per operation as the Python `hot()` loop:

| id | one operation | Python 0.12.0 per op |
|---|---|---|
| `py_s1_flat_toggle` | one `send("NEXT")` on A ⇄ B | 12.35 µs |
| `py_s2_guarded_context` | one guarded, counting `send("INC")` (build + start amortised over 20 000 sends) | 10.54 µs |
| `py_s3_hierarchical` | `send("CROSS")` + `send("DEEP")` across a two-level hierarchy | 28.37 µs |
| `py_s4_parallel` | `send("TOGGLE1")` + `send("TOGGLE2")` on two regions | 37.86 µs |
| `py_s5_construction` | `Machine::from_value` (hier) + `SyncInterpreter::new` + `start` | 79.0 µs |
| `py_s6_many_instances` | per instance of 1 000: `new` + `start`, then one `send` | 21.9 µs |
| `py_s7_delayed_transition` | new `SimulatedClock`, build, `new` + `start`, advance 1 ms, observe `done` | 118 µs |

The Python numbers are `median_s / n` from `benchmarks/competitors/results.json`
(`xstate-statemachine (sync)`, CPython 3.14.6, same i7-11850H host). Reproduce them with
`python benchmarks/competitors/run.py --lib xstate_sync` in the Python repository.

`crates/xstate-core/benches/core.rs` holds the older engine benchmarks:

| id | what it measures |
|---|---|
| `send_flat` | one event on a 2-state machine, no actions |
| `step_flat` | the bare `engine::step`, no interpreter |
| `send_guard_assign` | guard + JSON `assign` on a dynamic context |
| `send_nested_3` | 3-level hierarchy, cross-branch transition |
| `send_parallel_2` | two orthogonal regions toggling |
| `build_50_states` | parse + validate + compile 50-state JSON text |
| `build_phase_*` | the phases of `build_50_states`: JSON text to value, value to config, compile |
| `snapshot_get` | serialise the snapshot of a 50-state machine |
| `snapshot_restore` | parse + validate + restore |

`persisted_memory` (load, send, save on the memory store) has a target but no benchmark yet.
