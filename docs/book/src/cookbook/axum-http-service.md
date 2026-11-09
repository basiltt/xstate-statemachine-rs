# Recipe: Axum HTTP Service & Live Stately Inspector

Expose statechart actors over HTTP using `xstate-axum` with REST event endpoints, Server-Sent Events (SSE) state streaming, RFC 9457 Problem Details, and live visual debugging connected directly to Stately Studio.

---

## Architecture Flow

```mermaid
graph LR
    Client[Web / Mobile Client] -->|POST /api/machines/{id}/events| Axum[Axum HTTP Router]
    Client -->|GET /api/machines/{id}/stream (SSE)| Axum
    Stately[Stately Studio Inspector] -->|WebSocket /live| Axum
    Axum --> TokioActor[xstate-tokio Actor Mailbox]
```

---

## Rust Implementation

```rust
use axum::Router;
use std::sync::Arc;
use tokio::net::TcpListener;
use xstate_axum::{ActorRegistry, StatechartRouter};
use xstate_statemachine::prelude::*;
use xstate_tokio::{spawn, Options};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a machine
    let machine = Machine::from_json(r#"{
        "id": "toggle",
        "initial": "inactive",
        "states": {
            "inactive": {"on": {"TOGGLE": "active"}},
            "active": {"on": {"TOGGLE": "inactive"}}
        }
    }"#)?;

    // 2. Spawn actor and register in ActorRegistry
    let actor = spawn(machine, MachineLogic::dynamic(), Options::default())?;
    let registry = Arc::new(ActorRegistry::new());
    registry.register("toggle_singleton", actor);

    // 3. Assemble Axum Router with REST, SSE and Inspector endpoints
    let app = Router::new()
        .nest("/api/machines", StatechartRouter::new(registry).into_router());

    println!("Starting Axum statechart server on http://127.0.0.1:3000");
    let listener = TcpListener::bind("127.0.0.1:3000").await?;
    
    // Uncomment to run server:
    // axum::serve(listener, app).await?;

    Ok(())
}
```

---

## HTTP Endpoints Provided

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/machines/:id` | Returns the current state snapshot (JSON layout v4) |
| `POST` | `/api/machines/:id/events` | Dispatches an event payload; returns the `Receipt` |
| `POST` | `/api/machines/:id/events/:event` | Dispatches a named event URL parameter |
| `GET` | `/api/machines/:id/stream` | Server-Sent Events (SSE) channel pushing state updates |
| `GET` | `/api/machines/:id/inspect` | WebSocket endpoint for live visual inspection in Stately |

### Sample Response (`POST /api/machines/toggle_singleton/events`):
```json
{
  "state_ids": ["toggle.active"],
  "changed": true,
  "unhandled": false,
  "context": {},
  "mutations": 1
}
```

If an error occurs or invalid JSON is submitted, `xstate-axum` emits standard **RFC 9457 Problem Details** (`application/problem+json`).
