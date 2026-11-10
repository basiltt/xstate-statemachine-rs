# SeaORM Async ORM Integration

Persist and drive statecharts through [**SeaORM**](https://crates.io/crates/sea-orm) (15M+ downloads), the leading async, dynamic ORM for Rust built on SQLx.

---

## 🎯 Architecture: Statecharts as Entity ActiveModels

```mermaid
flowchart LR
    E[SeaORM Entity / Model] -->|Extract Snapshot JSON| S[Snapshot v4]
    S -->|Restore Configuration| I[SyncInterpreter]
    I -->|send event| R[Receipt]
    I -->|Serialize New Snapshot| S2[Updated Snapshot]
    S2 -->|Update state & snapshot fields| AM[ActiveModel]
    AM -->|save db transaction| DB[(PostgreSQL / MySQL / SQLite)]
```

In SeaORM applications, you store the high-level leaf state in a `state: String` column (for indexed SQL querying) and the full statechart execution configuration in a `snapshot: Json` column (to preserve parallel states, history, timers, and context).

---

## 📦 Dependencies

```toml
[dependencies]
sea-orm = { version = "1.1", features = ["sqlx-postgres", "runtime-tokio-rustls", "macros"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
xstate-statemachine = "0.1"
```

---

## 🛠️ Implementation: SeaORM Entity with Statechart Trait

### 1. Define the SeaORM Entity

```rust
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub order_ref: String,
    pub state: String,
    pub snapshot: Json,
    pub total_cents: i64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
```

### 2. Statechart Transition Extension Trait

```rust
use sea_orm::{DatabaseConnection, Set, ActiveModelTrait};
use xstate_statemachine::prelude::*;

pub trait StatechartModel {
    async fn send_event(
        self,
        db: &DatabaseConnection,
        machine: &Machine,
        logic: &MachineLogic,
        event: impl Into<Event>,
    ) -> Result<Model, Box<dyn std::error::Error>>;
}

impl StatechartModel for Model {
    async fn send_event(
        self,
        db: &DatabaseConnection,
        machine: &Machine,
        logic: &MachineLogic,
        event: impl Into<Event>,
    ) -> Result<Model, Box<dyn std::error::Error>> {
        // 1. Restore interpreter from entity's snapshot JSON
        let snapshot: Snapshot = serde_json::from_value(self.snapshot)?;
        let mut interp = SyncInterpreter::from_snapshot(machine.clone(), logic.clone(), snapshot)?;

        // 2. Drive the state transition
        let receipt = interp.send(event)?;
        if receipt.unhandled {
            return Err(format!("Event was unhandled in state: {}", self.state).into());
        }

        // 3. Serialize updated snapshot and primary leaf state
        let new_snapshot = interp.snapshot()?;
        let leaf_state = interp.state_ids().first().cloned().unwrap_or_default();

        // 4. Update the SeaORM ActiveModel
        let mut active: ActiveModel = self.into();
        active.state = Set(leaf_state.to_string());
        active.snapshot = Set(serde_json::to_value(new_snapshot)?);
        active.updated_at = Set(chrono::Utc::now());

        let updated_model = active.update(db).await?;
        Ok(updated_model)
    }
}
```

---

## 🚀 Driving Transitions in SeaORM

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::connect("postgres://xsm:xsm@localhost/shop").await?;
    let machine = Machine::from_json(ORDER_JSON)?;
    let logic = MachineLogic::dynamic();

    // Find order by ID
    let order = Entity::find_by_id(42)
        .one(&db)
        .await?
        .ok_or("Order not found")?;

    println!("Current order state: {}", order.state);

    // Apply event via statechart extension
    let updated_order = order.send_event(&db, &machine, &logic, "CONFIRM_PAYMENT").await?;

    println!("New order state: {}", updated_order.state);
    Ok(())
}
```
