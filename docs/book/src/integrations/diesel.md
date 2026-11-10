# Diesel Compile-Time ORM Integration

Type-safe statechart persistence with [**Diesel**](https://crates.io/crates/diesel) (30M+ downloads), Rust's leading compile-time ORM and query builder.

---

## 🎯 Architecture

```mermaid
flowchart LR
    D[Diesel Connection] -->|SELECT * FROM orders WHERE id = $1| Row[Database Row]
    Row -->|Deserialize snapshot column| S[Snapshot v4]
    S -->|Restore interpreter| I[SyncInterpreter]
    I -->|send event| R[Receipt]
    I -->|Snapshot update| S2[Updated Snapshot]
    S2 -->|diesel::update(orders).set(...)| D
```

Diesel gives compile-time SQL safety. By creating a custom SQL type mapping for `Snapshot`, statechart updates become transactional Rust operations executed without allocations.

---

## 📦 Dependencies

```toml
[dependencies]
diesel = { version = "2.2", features = ["postgres", "chrono", "serde_json"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
xstate-statemachine = "0.1"
```

---

## 🛠️ Schema & Model Setup

### 1. Diesel Schema (`schema.rs`)

```rust
diesel::table! {
    orders (id) {
        id -> Int4,
        order_ref -> Varchar,
        current_state -> Varchar,
        snapshot_data -> Jsonb,
        updated_at -> Timestamptz,
    }
}
```

### 2. Diesel Models

```rust
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde_json::Value;

#[derive(Queryable, Selectable, Identifiable, Debug)]
#[diesel(table_name = crate::schema::orders)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Order {
    pub id: i32,
    pub order_ref: String,
    pub current_state: String,
    pub snapshot_data: Value,
    pub updated_at: DateTime<Utc>,
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::orders)]
pub struct OrderUpdate<'a> {
    pub current_state: &'a str,
    pub snapshot_data: &'a Value,
    pub updated_at: DateTime<Utc>,
}
```

---

## 🚀 Transactional State Transition in Diesel

```rust
use diesel::prelude::*;
use xstate_statemachine::prelude::*;

pub fn transition_order(
    conn: &mut PgConnection,
    target_order_id: i32,
    machine: &Machine,
    logic: &MachineLogic,
    event: &str,
) -> Result<Order, Box<dyn std::error::Error>> {
    use crate::schema::orders::dsl::*;

    conn.transaction::<Order, Box<dyn std::error::Error>, _>(|conn| {
        // 1. Fetch order with row-level lock (FOR UPDATE)
        let order = orders
            .find(target_order_id)
            .for_update()
            .first::<Order>(conn)?;

        // 2. Restore statechart snapshot
        let snapshot: Snapshot = serde_json::from_value(order.snapshot_data)?;
        let mut interp = SyncInterpreter::from_snapshot(machine.clone(), logic.clone(), snapshot)?;

        // 3. Process event
        let receipt = interp.send(event)?;
        if receipt.unhandled {
            return Err(format!("Event '{}' rejected in state '{}'", event, order.current_state).into());
        }

        // 4. Serialize new snapshot
        let new_snapshot = interp.snapshot()?;
        let new_state = interp.state_ids().first().cloned().unwrap_or_default();
        let new_snapshot_json = serde_json::to_value(&new_snapshot)?;

        // 5. Update database record
        let updated = diesel::update(orders.find(target_order_id))
            .set(&OrderUpdate {
                current_state: &new_state,
                snapshot_data: &new_snapshot_json,
                updated_at: Utc::now(),
            })
            .get_result::<Order>(conn)?;

        Ok(updated)
    })
}
```
