# Event-driven architecture

`xstate-eda` (feature `eda` of the facade) connects machines to message brokers. It is a port of
the Python library's `eda` package and its broker adapters, and shares every wire format and table
layout with it, so Rust and Python services can publish to and consume from the same topics and
share one outbox or dead-letter database.

The pieces, from the wire inwards:

| Piece | What it does |
|---|---|
| `Envelope` | the CloudEvents 1.0 message: type, source, subject, id (sortable UUIDv7), time, data, causation / correlation ids, `traceparent`, attempt counter. Byte-identical JSON to Python |
| `BrokerAdapter` / `SyncBrokerAdapter` | one contract for every broker: `publish`, `poll`, `ack`, `nack`. Async and blocking flavours; `FakeBrokerAdapter` / `SyncFakeBrokerAdapter` for tests |
| `InboundDispatcher` | envelope to instance to event: inbox dedup, per-subject ordering, bounded retries, poison messages to dead letters |
| `OutboxPlugin`, `OutboxStore`, `OutboxRelay` | the transactional outbox: record the envelopes a transition declares next to the state, relay them to the broker later, at least once |
| `DeadLetter`, `DeadLetterPlugin`, stores, `BrokerDeadLetterSink` | what failed, why and with which snapshot; `replay_dead_letter` retries it |
| `asyncapi_document` | an AsyncAPI 3.0 document describing the events a chart consumes and publishes |

The crate is runtime-agnostic: the async contract uses return-position `impl Future + Send`, and
the dispatcher calls a handler function you supply instead of owning an interpreter.

## Consuming

A handler turns a delivered envelope into an event for the right instance. The usual one is
`persisted`, so every message is one load, send, save cycle ([persistence.md](persistence.md)):

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use xstate_statemachine::eda::{
    Dispatch, Envelope, HandlerError, InboundDispatcher, MemoryDeadLetterStore,
    SyncFakeBrokerAdapter, machines_by_type,
};
use xstate_statemachine::persistence::receipts::ReceiptFlags;
use xstate_statemachine::persistence::{MemoryInbox, MemoryStore, PersistedOptions, ReceiptJson, persisted};
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{"id":"counter","initial":"idle","context":{"total":0},
    "states":{"idle":{"on":{"ADD":{"actions":"add"}}}}}"#)?;
let logic = MachineLogic::dynamic().action("add", |scope| {
    let n = scope.event.payload["n"].as_i64().unwrap_or(0);
    scope.context["total"] = (scope.context["total"].as_i64().unwrap_or(0) + n).into();
    Ok(())
});

let store = Arc::new(MemoryStore::new());
let handler = {
    let store = Arc::clone(&store);
    move |d: &Dispatch<'_>| -> Result<ReceiptJson, HandlerError> {
        let receipt = persisted(&*store, d.key, d.machine, &logic, &PersistedOptions::default(), |interp, _| {
            let r = interp.send(d.event.clone())?;
            Ok(ReceiptJson::from_receipt(&r, ReceiptFlags::default(), None))
        })?;
        HandlerError::check_receipt(receipt) // an unhandled / errored event is a failure to retry
    }
};

// CloudEvents type `xsm.<machine id>.<EVENT>` selects the machine; the subject is the instance key.
let dispatcher = InboundDispatcher::new(
    machines_by_type(HashMap::from([("xsm.counter.ADD".to_owned(), machine)])),
    handler,
)
.inbox(Arc::new(MemoryInbox::new()))   // redeliveries are answered from the inbox
.max_attempts(3)?                      // then the envelope goes to the dead-letter store
.dead_letters(Arc::new(MemoryDeadLetterStore::new()));

let broker = SyncFakeBrokerAdapter::new();
let envelope = Envelope::new("xsm.counter.ADD").subject("c-1").data(serde_json::json!({"n": 5})).build()?;
broker.deliver("counters", &envelope)?;
broker.deliver("counters", &envelope)?;                 // the same message again
let result = dispatcher.run_once_sync(&broker, "counters", Duration::ZERO)?;
assert_eq!((result.processed, result.duplicates), (1, 1));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Guarantees: messages of one subject are handled in order (a failure puts the rest of that subject
back, in order, for redelivery); a message that fails `max_attempts` times (5 by default) is
dead-lettered and acked, so a poison message cannot block its subject; unknown types follow
`OnUnknown` (dead letter by default). `run_once` / `run_forever` drive an async broker,
`run_once_sync` / `run_forever_sync` a blocking one.

`ChoreographyRouter` (feature `patterns`) wraps this for the common case of a `type -> machine`
map. It drives blocking brokers only (`run_once` / `run_until_quiet` take a `SyncBrokerAdapter`).

**Async `run_once` handles, then acks (M8).** The handler is synchronous, so the async
`InboundDispatcher::run_once` pulls a batch (up to `max_batch`), runs the handlers of up to
`max_in_flight` subjects concurrently on scoped worker threads (one subject at a time, in order),
and only afterwards acks or requeues the deliveries of the batch. The calling task blocks while the
handlers run, so call it from a blocking-capable context (tokio's `block_in_place`) when handlers
are slow. Python acks each delivery as its handler finishes; a crash in the middle of a Rust batch
redelivers the batch and the inbox absorbs the duplicates. `run_once_sync` handles and settles
subject by subject.

## Publishing: the transactional outbox

Publishing straight to a broker inside a request can lose or duplicate messages. Declare what a
transition publishes in the chart (`meta.publish`, or tag a state `publish`), let `OutboxPlugin`
collect the envelopes as an observer, and write them to an `OutboxStore` in the same cycle that
saves the state; an `OutboxRelay` then ships them to the broker with claim leases, at least once.

```rust,ignore
let plugin = Arc::new(OutboxPlugin::new().instance_key("order-1"));
let options = PersistedOptions { observers: vec![plugin.clone() as Arc<dyn Observer<_>>], ..Default::default() };
persisted(&store, "order-1", &machine, &logic, &options, |interp, _session| {
    interp.send("PAY")?;
    plugin.flush_into(&outbox)?;          // same cycle as the state save
    Ok(())
})?;

let relay = OutboxRelay::new(outbox, broker).batch(100)?.lease_s(30.0)?;
relay.relay_once().await?;                // or relay_once_sync() with a blocking broker
```

(`flush_into` in the body is at-least-once with the relay; call it in `session.after_commit` if you
prefer never to record an envelope for a state that was not saved.) Delivery is **at-least-once**:
a crash between publishing and marking a row sent publishes it again, so consumers must dedup
(an inbox does). Stores: `MemoryOutboxStore` and `SqliteOutboxStore` (tables `xsm_outbox`, shared
with Python); there is no PostgreSQL outbox store.

## Dead letters

Two sources produce the same `DeadLetter` record: `DeadLetterPlugin` (an observer) captures a
poison message with its error chain when the chart enters a state tagged `dead-letter`, and the
dispatcher writes one for poison messages (after `max_attempts`), unknown event types (`unknown_event`),
corrupt envelopes (`corrupt`), deliveries for an instance that already finished (`instance_done`)
and an envelope id redelivered with different content (`idempotency_mismatch`). Records are redacted before they are written and carry the envelope, the reason, the
attempts and, optionally, a snapshot. The dispatcher's default in-memory store keeps the newest
10 000 records (oldest evicted first); pass a durable sink to `InboundDispatcher::dead_letters` to
keep everything. Stores: `MemoryDeadLetterStore`, `SqliteDeadLetterStore`
(tables `xsm_dead_letters` and the audit table `xsm_dlq_audit`), or `BrokerDeadLetterSink` to
publish them on a topic. `replay_dead_letter` re-sends a record through the dispatcher: it needs a
reason, is a dry run unless told otherwise, refuses a record written by a different machine
version unless forced, reuses the envelope id so an inbox absorbs a double replay, and audits the
action.

## AsyncAPI

```rust
use xstate_statemachine::eda::{AsyncApiOptions, asyncapi_document};
use xstate_statemachine::prelude::*;

let machine = Machine::from_json(r#"{"id":"counter","initial":"idle","states":{"idle":{"on":{"ADD":"idle"}}}}"#)?;
let document = asyncapi_document(&machine, &AsyncApiOptions::default())?; // AsyncAPI 3.0, serde_json::Value
assert_eq!(document["asyncapi"], "3.0.0");
# Ok::<(), Box<dyn std::error::Error>>(())
```

The document is the same as the one the Python library generates from the same chart, validated
against the AsyncAPI 3.0 schema bundled with the crate.

## Brokers

Each adapter implements both contracts on one type, so it works from async code and from threads.
Enable its feature, open it, and hand it to the dispatcher or the relay.

| Feature | Type | Opened with | Notes |
|---|---|---|---|
| `redis-streams` | `RedisStreamsBroker` | `open(url, &RedisStreamsOptions::new("app"))` | consumer groups, sharded streams, idle-message claiming; Python key and field layout |
| `nats` | `NatsBroker` | `open("nats://host:4222", &NatsOptions::new())` | JetStream streams, durable pull consumers, `Nats-Msg-Id` deduplication; needs a server started with `-js` |
| `rabbitmq` | `RabbitMqBroker` | `open("amqp://user:pass@host:5672/%2f", &RabbitMqOptions::new())` | durable queues, publisher confirms, `mandatory` publishes (an unroutable message is returned and reported, not dropped), delivery-count attempts |
| `sqs` | `SqsBroker` | `open(&SqsOptions::new())` | a queue per topic (never created by the adapter), FIFO groups and dedup ids for `.fifo` topics, long polling, visibility-timeout redelivery; credentials and region from the AWS environment, or `SqsOptions` for an emulator such as ElasticMQ |

Every broker's options carry an `io_timeout` (30 s by default): a poll, publish or ack that does
not complete in that time fails instead of hanging the consumer. Kafka is not supported yet. All four are exercised against real servers in CI (see the
[development section](../../README.md#development)) and, when the Python library is installed,
against its own adapters on the same server.
