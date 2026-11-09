# Cookbook & Example Gallery

Real-world, battle-tested recipe patterns showing how to construct, test, and operate statecharts with `xstate-statemachine-rs`.

Every recipe includes:
- The architectural statechart topology (JSON / diagrams).
- Full, copy-pasteable Rust implementation.
- Unit and integration tests demonstrating lifecycle behavior.
- Failure handling and production best practices.

---

## Recipes Index

1. **[Traffic Light Controller](traffic-light.md)**  
   *Core Engine & Basics.* Simple sequential state machine with entry/exit actions, typed enum events, and transition assertions.

2. **[Hierarchical Order Fulfillment](order-fulfillment.md)**  
   *Compound States & Context.* Multi-step e-commerce workflow with nested payment states, inventory reservation, and root-level cancellation handling.

3. **[Resilient Circuit Breaker & Exponential Backoff](circuit-breaker-retry.md)**  
   *Resilience Patterns.* Protecting fragile downstream HTTP services using `xstate-patterns` with failure counters, half-open probe testing, and decorrelated jitter.

4. **[Distributed E-Commerce Saga](distributed-saga.md)**  
   *Distributed Transactions.* Orchestrating multi-service payments, warehouse reservations, and shipments with automated compensating rollback actions.

5. **[Durable Persistence & Deadlines](durable-persistence.md)**  
   *Crash Recovery & Storage.* Storing statecharts in PostgreSQL and SQLite with optimistic concurrency control, idempotency tracking, and surviving server restarts.

6. **[Event-Driven Architecture & Transactional Outbox](eda-messaging.md)**  
   *CloudEvents & Brokers.* Dispatching and consuming events over NATS and RabbitMQ with transactional outbox relay guarantees and dead-letter queues.

7. **[Axum HTTP Microservice & Live Inspector](axum-http-service.md)**  
   *Web API & Observability.* Exposing statechart actors over REST and Server-Sent Events (SSE), with real-time visual inspection in Stately Studio.

8. **[LLM Agent Tool Loop with Guardrail Budgets](llm-agent-tool-loop.md)**  
   *AI Agents & Safety.* Constrained autonomous agent loop with strict per-state tool allow-lists, token budget guards, and human-in-the-loop approvals.
