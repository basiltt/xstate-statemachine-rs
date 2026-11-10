# SQLx & Database Persistence

Durable statechart persistence with [**SQLx**](https://crates.io/crates/sqlx) (45M+ downloads) for **PostgreSQL** and **SQLite**.

---

## 🏛️ Storage Architecture

```mermaid
flowchart TD
    App[Application Code] --> P[persisted() Transaction Wrapper]
    P -->|1. Acquire Lock| Lk[PG Advisory Lock / SQLite WAL Lock]
    P -->|2. Check Inbox| In[Idempotent Inbox: Deduplicate by SHA256 Key]
    P -->|3. Load Snapshot| St[xsm_snapshots: Binary Layout v4]
    P -->|4. Transition| E[xstate-core Microstep Engine]
    P -->|5. Save Snapshot| St
    P -->|6. Append Log| Log[xsm_transition_log: Audit Record]
    P -->|7. Enqueue Outbox| Ob[xsm_outbox: Transactional CloudEvents]
    P -->|8. Release Lock| Lk
```

All table schemas and byte layouts are **100% binary-compatible with the Python library** (`xstate-statemachine` 0.12.1). A database created by Python can be resumed by Rust without any schema migration.

---

## 📦 Dependencies

```toml
[dependencies]
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "sqlite"] }
xstate-statemachine = { version = "0.1", features = ["persistence", "postgres", "sqlite"] }
```

---

## 🐘 PostgreSQL Setup & Usage

### 1. Connection Pool & Migrations

```rust
use sqlx::postgres::PgPoolOptions;
use xstate_statemachine::prelude::*;
use xstate_statemachine::persistence::{PostgresStore, PostgresStoreOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect("postgres://xsm:xsm@localhost:5432/xsm").await?;

    // PostgresStore automatically runs necessary migrations on startup
    let store = PostgresStore::connect_with_pool(pool, PostgresStoreOptions::default()).await?;

    // Load or initialize machine
    let machine = Machine::from_json(ORDER_JSON)?;
    let logic = MachineLogic::dynamic();

    // Persist with distributed advisory lock
    let receipt = store.transaction("order-1001", &machine, &logic, |interp| {
        interp.send("APPROVE")
    }).await?;

    println!("Order moved to: {:?}", receipt.state_ids());
    Ok(())
}
```

### 2. SQL Schema Created by `xstate-persistence`

```sql
CREATE TABLE IF NOT EXISTS xsm_snapshots (
    key VARCHAR(255) PRIMARY KEY,
    snapshot JSONB NOT NULL,
    version BIGINT NOT NULL,
    structure_hash VARCHAR(16) NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS xsm_inbox (
    key VARCHAR(255) NOT NULL,
    idempotency_key VARCHAR(255) NOT NULL,
    receipt JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (key, idempotency_key)
);

CREATE TABLE IF NOT EXISTS xsm_transition_log (
    id BIGSERIAL PRIMARY KEY,
    key VARCHAR(255) NOT NULL,
    event VARCHAR(255) NOT NULL,
    from_states TEXT[] NOT NULL,
    to_states TEXT[] NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

---

## 🪶 SQLite Setup & Embedded Operation

For single-node microservices, CLI tools, or edge devices, SQLite in WAL mode delivers microsecond persistence:

```rust
use xstate_statemachine::prelude::*;
use xstate_statemachine::persistence::{SqliteStore, SqliteStoreOptions, JournalMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = SqliteStoreOptions::default()
        .journal_mode(JournalMode::Wal)
        .busy_timeout_ms(5000);

    let store = SqliteStore::open("workflows.db", options)?;

    let machine = Machine::from_json(WORKFLOW_JSON)?;
    let logic = MachineLogic::dynamic();

    // Synchronous execution with zero async overhead
    let receipt = persisted(
        &store,
        "wf-001",
        &machine,
        &logic,
        &PersistedOptions::default(),
        |interp, _| {
            interp.send("STEP_COMPLETE")?;
            Ok(interp.state_ids())
        },
    )?;

    println!("Workflow states: {:?}", receipt);
    Ok(())
}
```
