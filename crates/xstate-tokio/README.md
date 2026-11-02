# xstate-tokio

Asynchronous interpreter for `xstate-core` machines on the [tokio](https://tokio.rs) runtime.
`spawn` runs a machine as a task that owns its state and drives the sans-I/O step engine: a
bounded mailbox with overflow policies, `after` timers on `tokio::time` (paused time works in
tests), asynchronous services, child machine actors, `sendTo` / `sendParent` routing and a
`systemId` registry. The returned `Interpreter` is a cheap, cloneable handle.

Status: 0.1.0. The sync and async interpreters share one algorithm (ADR-0004), so behaviour is
identical to `SyncInterpreter` apart from scheduling.

```rust
use xstate_core::{Machine, MachineLogic};
use xstate_tokio::{Options, spawn};

#[tokio::main(flavor = "current_thread")]
async fn main() -> xstate_core::Result<()> {
    let machine = Machine::from_json(r#"{"id":"t","initial":"off","states":{
      "off":{"on":{"TOGGLE":"on"}},"on":{"on":{"TOGGLE":"off"}}}}"#)?;
    let actor = spawn(machine, MachineLogic::dynamic(), Options::default())?;
    actor.send_and_wait("TOGGLE").await?;
    assert_eq!(actor.view().state_ids, ["t.on"]);
    actor.stop().await;
    Ok(())
}
```

## Features

None. Depends on tokio with `rt`, `sync`, `time` and `macros`; you choose the runtime flavour.

## Python counterpart

[`xstate_statemachine.interpreter`](https://github.com/basiltt/xstate-statemachine/blob/main/src/xstate_statemachine/interpreter.py)
(the asyncio `Interpreter`), `actor_logic.py` and `task_manager.py`.
