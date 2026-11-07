# Deployment and scaling

The library is embedded, not a standalone service: you build your application and link it. This
page collects the deployment-relevant facts that are grounded in the code.

## Choose the feature set you ship

The facade crate (`xstate-statemachine`) has everything off by default; each feature adds a module
and a dependency tree (see [feature flags](../guide/features.md)). Ship only what you use: the engine
alone has no I/O, no async runtime and compiles to `wasm32`. `sqlite` bundles SQLite (a C
dependency); `postgres` needs Rust 1.94 because of `sqlx` 0.9; every other feature honours the
1.88 MSRV.

## The library speaks plain HTTP

The axum integration and the inspector SSE server serve plain HTTP — terminate TLS in a reverse
proxy or load balancer and keep the inspector bound to loopback (it only accepts loopback `Host`
headers there; a non-loopback bind requires an explicit token). There is **no default authorizer**:
`RegisterOptions::new(authorizer)` is required, and `AllowAll` is an explicit development opt-out
that logs a warning. See [security hardening](security.md).

## Concurrency and the request / response model

The sync interpreter and the `StateStore` / inbox / log traits are blocking. The tokio
interpreter gives you actors (`spawn`), and from async code you call blocking stores inside
`tokio::task::spawn_blocking`. The intended service pattern is *create, act, persist, discard*:
`persisted` loads a snapshot, runs your closure, saves with a version fence and drops the
interpreter, so a request-driven worker holds no in-memory state between requests (see
[persistence](../guide/persistence.md)).

## Scaling out

- Multiple workers can share one store: `OptimisticLock` (save fails with `StoreError::Conflict`)
  or `PessimisticLock` (per-key lock with a TTL on Redis) coordinate writers. `persisted_retry`
  re-runs the closure with jittered backoff.
- Durable timers need one scanner somewhere: `DueTimerScanner` fires matured deadlines under a
  lock; run it from a scheduler (`run_once`) or a loop (`run_until`).
- Backends are configured by environment variables (see
  [environment variables](../reference/env-vars.md)) and are started with Docker in development;
  the exact containers are documented in the root `README.md`.

## Process hardening

Every crate forbids `unsafe` code (`unsafe_code = "forbid"`), and CI runs cargo-deny and
cargo-audit. The machine JSON parser never panics on untrusted input — it returns typed errors —
and snapshot restore caps its input at `DEFAULT_MAX_INPUT_BYTES` (1 MiB); cap any caller-accepted
text at the transport too.
