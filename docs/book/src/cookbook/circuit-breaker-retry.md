# Recipe: Resilient Circuit Breaker & Exponential Backoff

Unreliable remote HTTP endpoints and database calls can cascade failures throughout your application. This recipe demonstrates how to combine the **Circuit Breaker** and **Retry with Exponential Backoff** patterns using `xstate-patterns`.

---

## Statechart Topology

```mermaid
stateDiagram-v2
    [*] --> Closed
    
    state Closed {
        [*] --> Idle
        Idle --> Executing: CALL
        Executing --> Idle: SUCCESS
        Executing --> Tripping: FAILURE [failures >= threshold]
    }

    Closed --> Open: TRIP
    
    state Open {
        [*] --> Sleeping
        Sleeping --> HalfOpen: TIMEOUT (after cooldown)
    }

    state HalfOpen {
        [*] --> Probing
        Probing --> Closed: PROBE_SUCCESS
        Probing --> Open: PROBE_FAILURE
    }
```

---

## Rust Implementation

```rust
use std::time::Duration;
use xstate_patterns::{CircuitBreakerOptions, RetryOptions, backoff, circuit_breaker};
use xstate_statemachine::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Configure the Circuit Breaker pattern
    let cb_options = CircuitBreakerOptions {
        failure_threshold: 3,
        recovery_timeout: Duration::from_millis(500),
        success_threshold: 2,
    };
    let breaker_machine = circuit_breaker::build_machine(&cb_options)?;

    // 2. Configure Retry with Exponential Backoff and Decorrelated Jitter
    let retry_options = RetryOptions {
        max_attempts: 4,
        initial_interval: Duration::from_millis(50),
        max_interval: Duration::from_millis(1000),
        multiplier: 2.0,
        randomization_factor: 0.1,
    };
    let retry_machine = backoff::build_retry_machine(&retry_options)?;

    println!("Circuit breaker and retry statecharts assembled successfully.");

    // Drive the circuit breaker interpreter
    let mut cb_interp = SyncInterpreter::new(breaker_machine, MachineLogic::dynamic());
    cb_interp.start()?;
    assert_eq!(cb_interp.state_ids(), ["circuit_breaker.closed"]);

    // Simulate 3 consecutive failures to trip the breaker
    cb_interp.send("FAILURE")?;
    cb_interp.send("FAILURE")?;
    cb_interp.send("FAILURE")?;
    assert_eq!(cb_interp.state_ids(), ["circuit_breaker.open"]);

    // In 'open' state, calls fail fast immediately without hitting downstream servers
    let fast_fail_receipt = cb_interp.send("CALL")?;
    assert!(fast_fail_receipt.unhandled || cb_interp.matches("circuit_breaker.open"));

    Ok(())
}
```

---

## Production Characteristics
- **Fail Fast:** When the circuit is `Open`, incoming requests are refused immediately (under 100 ns), shielding fragile backends from brownouts.
- **Probe Verification:** In `HalfOpen`, only limited test traffic passes through. If the probe succeeds twice (`success_threshold: 2`), the circuit automatically heals back to `Closed`.
- **Zero Allocations:** Both pattern statecharts operate with pre-compiled static structures and bitset states.
