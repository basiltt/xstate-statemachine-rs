# §6.2 Battle test — xstate-eda

Scope: the §6.2 `xstate-eda` matrix in `docs/HANDOVER-QA.md` — CloudEvents 1.0 specification
rules, extension names, traceparent grammar, size and depth limits, adversarial dispatcher
runs (ordering, retry caps, dead-letter store outage, panicking handlers and hooks, custom
sinks, replay), transactional outbox (concurrent relays, lease expiry, crash between claim and
publish), and real broker adapters with Docker containers (Redis Streams, NATS JetStream,
RabbitMQ, AWS SQS on ElasticMQ).

Severity counts: **0 CRITICAL · 1 HIGH · 0 MEDIUM · 0 LOW** (one panic-containment defect found
and fixed in `handle()`; everything else sound).

New test files:

- `crates/xstate-eda/tests/dispatcher_adversarial.rs` — 11 adversarial tests covering panicking
  sinks and hooks, zero/bad limits, prefixing, inbox scoping, `instance_done`, replay audit logs.
- `crates/xstate-eda/tests/envelope_adversarial.rs` — 15 adversarial tests covering CloudEvents
  attribute validation, size/depth caps, malformed JSON, and Python envelope compatibility.
- `crates/xstate-eda/tests/outbox_adversarial.rs` — 5 adversarial tests covering zero/NaN leases,
  crash recovery, mid-store failures, and concurrent relay deduplication.

## Findings

### HIGH-1 — A panicking DeadLetterSink::put escapes handle() and crashes the caller

**Status: FIXED in `crates/xstate-eda/src/dispatcher/handle.rs`.**

- **file:line**
  - `crates/xstate-eda/src/dispatcher/handle.rs:272` — `dead_letter` invoked `self.dead_letters.put(record)`
    without `catch_unwind`. If the dead letter sink panics (e.g. a bug in a custom sink closure or
    store plugin), `handle_inner` unwound to `handle()`'s catch_unwind as `Flow::Counted`. `handle()`
    then called `outside_failure()`, which re-invoked `self.dead_letter()` outside of `catch_unwind`.
    The second panic escaped `handle()`, violating the documented contract that `handle()` "never fails
    and never panics on a panicking handler or hook".

- **reproduction**
  ```powershell
  cargo test -p xstate-eda --test dispatcher_adversarial a_panicking_dead_letter_sink_does_not_make_handle_panic
  ```

- **observed vs expected**
  - Observed: `handle()` panicked with `sink bug` escaped to the caller.
  - Expected: `handle()` catches the sink failure/panic, logs the error, requeues the delivery
    as an infrastructure failure (`Flow::Requeue`), and increments `res.retried` without panicking.

- **fix**
  `dead_letter()` in `crates/xstate-eda/src/dispatcher/handle.rs` now wraps `self.dead_letters.put(record)`
  in `catch_unwind(AssertUnwindSafe(...))`. If `put` panics, it returns `Flow::Requeue` with the panic
  message, safely requeuing the delivery without ever panicking.

## Verified sound

- **CloudEvents 1.0 specification adherence**: attribute names, kebab-case and snake_case extensions,
  traceparent format, RFC-3339 timestamps, UTF-8 payloads, and depth limits.
- **Dispatcher idempotency and routing**: scoped inbox claims (`source/machine/key`), dedup key
  extraction, attempt bounding, poison event quarantine, and replay safety.
- **Transactional outbox**: memory and SQLite outbox implementations, lease-based concurrency
  between competing relays, crash recovery between claim and publish, and poison row isolation.
- **Broker adapters with live Docker services**:
  - Redis Streams (`xsm-redis`): consumer groups, stream trimming, message reclamation.
  - NATS JetStream (`xsm-nats`): stream auto-provisioning, subject tokenization, ack/nack semantics.
  - RabbitMQ (`xsm-rabbit`): quorum queues, delivery count tracking, dead letter exchanges.
  - SQS (`xsm-sqs`): FIFO deduplication, message group IDs, visibility timeouts.
  - Cross-language: bidirectional publish and consume between Python 0.12.1 and Rust across all brokers.
- **Total tests**: 230 tests passed across unit, integration, and broker suites.
