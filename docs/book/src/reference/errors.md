# Error catalogue

Errors are typed enums, not strings. Each crate owns the errors its surface raises; receipts carry
an `ErrorKind` instead of an exception class name, and the HTTP layer maps them to RFC 9457
problem details (never exception text).

This page is a pointer to the catalogue rather than a reproduction of it — the definitive list of
variants, fields and messages is the rustdoc under `/api/` (published with the site).

| Crate | Error type | Notes |
|---|---|---|
| `xstate-core` | `Error` with `ErrorKind` | `InvalidConfig`, `RootTarget`, `StateNotFound`, `ImplementationMissing`, `InvalidEvent`, `UnknownEvent`, `InvalidLifecycle`, `TransitionFailed`, `RunawayChain`, `ActorSpawning`, `UnresolvedTarget`, `LimitExceeded`, `Snapshot`, `Logic` |
| `xstate-persistence` | `StoreError` | load/save/version/conflict/lock failures, schema-version refusal, key/value validation |
| `xstate-eda` | `EdaError`, `BrokerError` | envelope validation, dispatcher, outbox, dead letters, broker adapters |
| `xstate-patterns` | `RetryError` | retry-policy construction and use |
| `xstate-agents` | `AgentError` | tool, budget, approval and trace failures |

## What the user sees

- A name the chart uses but the registry lacks is `ImplementationMissing` at `start()`, naming the
  guard / action / service / delay — before any event runs.
- A guard or action failure surfaces through the `Receipt`'s `error` (`ErrorKind`), and the HTTP
  routes translate it to a problem-details response with the same titles as the Python library.
- Parser and restore failures are values, never panics: hostile JSON returns `InvalidConfig`,
  `Snapshot` or `LimitExceeded`.

For the full variant list and the exact status-code mapping, see the rustdoc and the
[security hardening](../operations/security.md) page (which covers the failure modes on the HTTP
surfaces).
