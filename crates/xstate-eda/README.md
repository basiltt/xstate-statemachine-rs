# xstate-eda

Event-driven architecture for `xstate-core` machines: CloudEvents 1.0 envelopes (byte-identical
JSON to the Python library, sortable UUIDv7 ids), the broker contract with an in-memory fake, the
inbound dispatcher (inbox dedup, per-subject ordering, bounded retries, poison messages to dead
letters), the transactional outbox with a relay, dead-letter stores and AsyncAPI 3.0 generation.

SQLite tables (`xsm_outbox`, `xsm_dead_letters`, `xsm_dlq_audit`) and every JSON shape are the
Python library's, so a database or a broker can be shared between the two runtimes. The crate is
runtime-agnostic: the async broker contract uses return-position `impl Future + Send`.

Status: 0.1.0. Kafka is not yet supported.

```rust
use serde_json::json;
use xstate_eda::Envelope;

let envelope = Envelope::new("order.paid").subject("order-1").data(json!({"total": 42})).build()?;
let wire = envelope.to_json(1 << 20)?;
assert_eq!(Envelope::from_json(&wire, 1 << 20)?.ty, "order.paid");
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

| Feature | Adds |
|---|---|
| `sqlite` (default) | `SqliteOutboxStore`, `SqliteDeadLetterStore` |
| `redis-streams` | `RedisStreamsBroker` (consumer groups, Python key and field layout) |
| `nats` | `NatsBroker` (JetStream, durable pull consumers, `Nats-Msg-Id` dedup) |
| `rabbitmq` | `RabbitMqBroker` (durable queues, publisher confirms) |
| `sqs` | `SqsBroker` (Amazon SQS and ElasticMQ, visibility-timeout redelivery) |

## Python counterpart

[`xstate_statemachine.eda`](https://github.com/basiltt/xstate-statemachine/tree/main/src/xstate_statemachine/eda),
`patterns.dead_letter` and `contrib.brokers`.
