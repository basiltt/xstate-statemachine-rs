# Security hardening and threat model

This page is the threat model behind [`SECURITY.md`](https://github.com/basiltt/xstate-statemachine-rs/blob/main/SECURITY.md). It covers the
surfaces the library itself owns when it is embedded in an application. Deployment hardening
(TLS termination, network policy, secret storage) belongs to the operator and is summarised at
the end.

## Assets

- **Machine definitions** — XState JSON documents and their structure hashes.
- **Context and event payloads** — application data, which may include personal data or
  credentials the application chose to put there.
- **Snapshots** — persisted configuration, context, pending events, history and deadlines.
- **Durability records** — durable timers, idempotency inbox entries, dead letters, audit and
  transition logs, transactional-outbox rows.
- **Inspector streams** — the full event/transition stream of a running machine.
- **Availability** of the host process: the parser, inspectors and HTTP surfaces all accept
  data from outside the application.

## Trust boundaries

1. **Untrusted bytes to the JSON model.** Machine documents, snapshots and CloudEvents
   envelopes arrive as text. `serde_json` enforces its default 128-level recursion limit and
   every parsed value is validated into a typed model; errors are values, not panics.
2. **Untrusted clients to the HTTP/WebSocket/SSE surfaces.** The axum integration and the
   inspector server accept network input from clients the application may not control.
3. **Application to storage.** Keys, snapshots and payloads passed to `xstate-persistence`
   and `xstate-eda` stores are validated before they reach a backend.
4. **Process to broker.** Broker credentials and endpoint URLs are operator configuration;
   the library does not fetch URLs derived from machine documents.

## Threats and controls

| Threat | Control | Where |
|---|---|---|
| Parser stack exhaustion or panic from a hostile document | `serde_json`'s 128-level cap for text; a 128-state-level nesting bound for programmatically built values, returned as `LimitExceeded`; the parser never panics on accepted input | `crates/xstate-core/src/config/parse.rs`, `machine/direct.rs`, `machine/compile.rs` |
| Oversized documents or snapshots | `DEFAULT_MAX_INPUT_BYTES` (1 MiB) is enforced by snapshot restore and the CLI; HTTP bodies use `DEFAULT_MAX_SNAPSHOT_BYTES`; `Machine::from_json` itself does not impose a size cap, so a caller that accepts untrusted text must cap it at the transport | `crates/xstate-core/src/lib.rs`, `snapshot.rs`, `crates/xstate-axum/src/body.rs` |
| Restoring a tampered or stale snapshot | schema-version and machine-hash checks (`SnapshotVersionError`, `expected_machine_hash`), corruption surfaced as `SnapshotCorruptError`/`Snapshot`, never silently accepted | `crates/xstate-core/src/snapshot.rs`, `interpreter/restore.rs` |
| Malformed CloudEvents envelope | required-attribute and grammar validation, `NaN`/`Infinity` refused, per-transport size cap through `from_json_bytes(max_bytes)` | `crates/xstate-eda/src/envelope*`, `async_core.rs` |
| DNS rebinding against the inspector | loopback binds only accept loopback `Host` headers, parsed strictly; non-loopback binds require an explicit token | `crates/xstate-inspect/src/sse/auth.rs` |
| Cross-site requests to the inspector | every request carrying `Origin` must be same-origin or allow-listed; the token is also accepted as an `HttpOnly` cookie after the first page load, and `?token=` is honoured only there | `crates/xstate-inspect/src/sse/auth.rs` |
| Token guessing | 32 bytes of entropy, constant-time comparison, no token echoed in responses or logs | `crates/xstate-inspect/src/sse/auth.rs` |
| Unauthorised access to machine instances over HTTP | there is **no default authorizer**: `RegisterOptions::new(authorizer)` is required, `AllowAll` is an explicit opt-out that logs a warning, and authorization runs before existence checks so keys cannot be enumerated | `crates/xstate-axum/src/auth.rs`, `routes*` |
| Cross-site POSTs / form-based request forgery | state-changing routes check `Origin` and require `application/json`; WebSocket events pass the same per-event gates and only declared events are accepted | `crates/xstate-axum/src/` |
| Connection or body exhaustion | body size cap (413) and body timeout (408), per-key and registry-wide connection caps (SSE 429 / WebSocket close 1013), WebSocket send timeouts | `crates/xstate-axum/src/body.rs`, `registry/connections.rs` |
| Secret leakage into logs, traces or inspector messages | payload allow-lists, `redact` helpers for context/exception rendering, and no requirement to log raw payloads | `crates/xstate-core/src/redact.rs`, `xstate-inspect`, `xstate-observability` |
| Store-level data damage | key validation, value-size caps, lock files with stale-holder detection, schema-version refusal, transactional writes | `crates/xstate-persistence/src/`, `crates/xstate-eda/src/` |
| Memory-unsafety and supply-chain compromise | `#![forbid(unsafe_code)]` in every crate; `cargo-deny` (advisories, bans, licences, sources) and `cargo-audit` in CI | workspace, `.github/workflows/ci.yml` |

## Durable-timer and idempotency notes

Durable timers and inbox entries are keyed by application-chosen keys under a store prefix.
Keys are validated (`/` scope separators are refused where the Python library refuses them) so a
key cannot escape its prefix. Idempotency fingerprints refuse payload text that embeds a memory
address, so a retry is not mistaken for a replay of a different payload.

## Not provided by the library

- **TLS.** The axum integration and the inspector server speak plain HTTP. Terminate TLS in a
  reverse proxy or load balancer, and keep the inspector bound to loopback.
- **Authentication of end users.** The authorizer is a hook; a principal is whatever the
  application's middleware puts into the request extensions.
- **Encryption at rest.** Stores persist snapshots as the backend writes them; use an encrypted
  volume or the backend's own encryption if context carries sensitive data.
- **Sandboxing of user logic.** Actions, guards and services run in-process with the
  application's privileges. Machine documents are data, but any logic the application registers
  is code the application chose to run.

## Reporting a vulnerability

See [`SECURITY.md`](https://github.com/basiltt/xstate-statemachine-rs/blob/main/SECURITY.md) for the private reporting channel and expected
response times.
