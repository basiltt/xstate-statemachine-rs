# xstate-patterns

Resilience patterns for `xstate-core` machines. Each one is a thin layer over the statechart
engine rather than a runtime of its own, and the machine JSON they emit is byte-for-byte what the
Python builders emit, so charts can be inspected, diffed and edited with the same tooling.

| Module | Pattern | What it gives you |
|---|---|---|
| `retry` | `RetryPolicy` | exponential backoff with jitter as named delay / guard / actions; decorrelated jitter keeps the previous delay in the context under `<attempt_key>_delay_ms` |
| `circuit_breaker` | `CircuitBreaker` | Nygard's breaker as a three-state chart on a `SyncInterpreter`, applied with `wrap(breaker, f)` (there is no decorator) |
| `saga` | `SagaBuilder` | orchestrated sagas emitted as plain XState machine JSON |
| `choreography` | `ChoreographyRouter` | an inbound dispatcher over a `type -> machine` map (blocking brokers only) |

Status: 0.1.0; goldens generated from the Python library pin the saga and circuit-breaker charts.

```rust
use xstate_patterns::{JitterMode, RetryPolicy};

let policy = RetryPolicy::new(3, 100.0, 2.0, 1_000.0, JitterMode::None)?;
assert_eq!(policy.delay_for(1)?, 100.0);
assert_eq!(policy.delay_for(2)?, 200.0);
assert!(policy.should_retry(2));
assert!(!policy.should_retry(3));
# Ok::<(), xstate_patterns::RetryError>(())
```

## Features

None. Depends on `xstate-eda` and `xstate-persistence` without their optional backends.

## Python counterpart

[`xstate_statemachine.patterns`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/patterns)
(`retry`, `circuit_breaker`, `saga`, `choreography`; `dead_letter` lives in `xstate-eda`).
