# Recipe: Stripe Webhook Payment Gateway

Idempotent Stripe webhook handling using `xstate-statemachine` and `xstate-persistence`.

---

## 🎯 Statechart Topology

```mermaid
statechart
    [*] --> Pending
    Pending --> Validating: STRIPE_EVENT_RECEIVED
    Validating --> Paid: payment_intent.succeeded
    Validating --> Failed: payment_intent.payment_failed
    Validating --> Refunded: charge.refunded
    
    Paid --> [*]
    Failed --> [*]
    Refunded --> [*]
```

---

## 🛠️ Complete Implementation with Idempotency Guard

```rust
use std::sync::Arc;
use xstate_statemachine::prelude::*;
use xstate_statemachine::persistence::{SqliteStore, persisted, PersistedOptions};

const STRIPE_MACHINE: &str = r#"{
    "id": "stripe_invoice",
    "initial": "pending",
    "context": {"invoice_id": null, "amount_cents": 0},
    "states": {
        "pending": {
            "on": {
                "PAYMENT_SUCCEEDED": {
                    "target": "paid",
                    "actions": "recordPayment"
                },
                "PAYMENT_FAILED": {
                    "target": "failed",
                    "actions": "recordFailure"
                }
            }
        },
        "paid": {"type": "final"},
        "failed": {"type": "final"}
    }
}"#;

pub fn handle_stripe_webhook(
    store: &SqliteStore,
    machine: &Machine,
    logic: &MachineLogic,
    event_id: &str, // Stripe Event ID: "evt_1N..." used as idempotency key
    invoice_id: &str,
    event_type: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut options = PersistedOptions::default();
    options.idempotency_key = Some(event_id.to_string());

    // persisted() automatically enforces idempotency:
    // If Stripe sends the same webhook twice, the duplicate is skipped and the cached receipt returned.
    let states = persisted(
        store,
        invoice_id,
        machine,
        logic,
        &options,
        |interp, _| {
            match event_type {
                "payment_intent.succeeded" => interp.send("PAYMENT_SUCCEEDED")?,
                "payment_intent.payment_failed" => interp.send("PAYMENT_FAILED")?,
                _ => return Ok(interp.state_ids()),
            };
            Ok(interp.state_ids())
        },
    )?;

    Ok(states)
}
```
