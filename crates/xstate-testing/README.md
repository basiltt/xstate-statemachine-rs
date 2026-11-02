# xstate-testing

Test helpers for `xstate-core` machines: stub logic that records executed actions, simulated-clock
fixtures, path walking and statechart coverage.

Status: 0.1.0, **skeleton**. This crate reserves the name and the home for these helpers; today
the working equivalents live in `xstate-core`: `graph::stub_logic` (every named guard, action and
service stubbed), `graph::{reachable_states, shortest_paths, simple_paths}`,
`SimulatedClock`, and the transition / state coverage targets. The `xsm paths` and
`xsm simulate` commands are built on them.

```rust
use xstate_core::Machine;
use xstate_core::graph::{GraphOptions, GuardPolicy, shortest_paths, stub_logic};

let machine = Machine::from_json(r#"{"id":"m","initial":"a","states":{
  "a":{"on":{"GO":{"target":"b","guard":"ok"}}},"b":{}}}"#)?;
let logic = stub_logic::<xstate_core::DynContext>(&machine, GuardPolicy::True);
let paths = shortest_paths(&machine, &logic, &GraphOptions::default())?;
assert_eq!(paths.len(), 2); // the initial configuration and `b`
# Ok::<(), xstate_core::Error>(())
```

## Features

None.

## Python counterpart

[`xstate_statemachine.testing_utils`](https://github.com/basiltt/xstate-statemachine/blob/main/src/xstate_statemachine/testing_utils.py),
`coverage.py` and `contrib.testing` (the pytest plugin has no Rust counterpart: use `cargo test`).
