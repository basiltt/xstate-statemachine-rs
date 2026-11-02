# xstate-axum

[axum](https://github.com/tokio-rs/axum) integration for `xstate-core` machines: the Rust
counterpart of the Python library's Starlette registry and FastAPI router.

* `StatechartRegistry`: named machines over one `StateStore`; every request is one create, act,
  persist, discard cycle through `persisted`, with an authorizer, a principal-scoped idempotency
  inbox, post-save fan-out to stream subscribers, probes, the durable-timer scanner and a
  graceful drain.
* `router`: `GET /{name}/{id}`, `POST /{name}/{id}/send`, `POST /{name}/{id}/events/{E}`,
  `GET /{name}/{id}/events`, `GET .../diagram.mmd`, `GET .../stream` (SSE), `GET .../ws`. The
  WebSocket enforces the same per-event gates (`RouterOptions::per_event_gates`) as the HTTP
  routes, and the POST routes check the `Origin` header like the streams do.
* `problem()` / `ApiError`: RFC 9457 problem details with the Python titles, never exception text.
* `inspector_router`: the Stately Inspector over WebSocket, debug-only and token-gated.

Status: 0.1.0.

```rust,no_run
use std::sync::Arc;
use xstate_axum::{RegisterOptions, RouterOptions, StatechartRegistry, allow_all, router};
use xstate_core::{Machine, MachineLogic};
use xstate_persistence::MemoryStore;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let machine = Machine::from_json(
    r#"{"id":"order","initial":"pending","states":{"pending":{"on":{"PAY":"paid"}},"paid":{}}}"#,
)?;
// RegisterOptions has no `Default`: the authorizer is a required argument, so a registry
// cannot be exposed unauthenticated by accident. `allow_all` is for development only; pass your
// own Authorizer in production.
let options = RegisterOptions::new(allow_all());
let registry = StatechartRegistry::builder(Arc::new(MemoryStore::new()))
    .register("order", machine, MachineLogic::dynamic(), options)?
    .build()?;
registry.start()?;
let app = router(&registry, "order", RouterOptions::default())?.merge(registry.probes_router());
axum::serve(tokio::net::TcpListener::bind("127.0.0.1:8000").await?, app).await?;
# Ok(()) }
```

## Features

None.

## Python counterpart

[`contrib.starlette`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/contrib/starlette)
and [`contrib.fastapi`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/contrib/fastapi).
