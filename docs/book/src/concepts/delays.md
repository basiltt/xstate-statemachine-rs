# Delays (`after`)

`after` transitions schedule a timer. The delay is either a number of milliseconds or the name of
a delay registered with `MachineLogic::delay(name, ms)`; delay functions
(`Fn(&C, &Event) -> Result<f64>`) compute it from context and event.

`SyncInterpreter` runs timers on a clock you control. Pass a `SimulatedClock` to `with_clock` and
move time with `interp.advance(&clock, ms)` — tests never sleep. `RealClock` (the default) uses
the wall clock. The tokio interpreter drives `after` on `tokio::time`, so `tokio::time::pause()`
works in tests.

Numeric delays are normalised (`"1e3"` → 1000 ms), and the timer event, structure hash, snapshots
and diagrams all use the normalised value (`xstate.after.1000.<id>`). A delay function that fails
or returns NaN/infinity disables that one timer with an error log; an unregistered delay *name* is
an `ImplementationMissing` error at `start()`.

Persistence stores the deadlines in the snapshot, and a `DueTimerScanner` fires matured timers
across process boundaries — see the [persistence guide](../guide/persistence.md).
