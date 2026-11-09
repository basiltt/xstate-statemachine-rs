# §6.2 Battle test — xstate-axum

Scope: the §6.2 `xstate-axum` matrix in `docs/HANDOVER-QA.md` — adversarial HTTP / WebSocket /
SSE over every public route, RFC 9457 problem details, header-only idempotency, Origin/Host rules,
body limits and timeouts, the registry lifecycle, and the Inspector endpoint. This pass adds the
battle suite on top of the existing `http.rs` / `lifecycle.rs` / `streaming.rs`.

Severity counts: **0 CRITICAL · 1 HIGH · 0 MEDIUM · 0 LOW** (one parity/correctness defect found;
everything else in the matrix exercised and sound — see "Verified sound").

New test files (kept as added coverage; the one that found the bug is marked):

- `crates/xstate-axum/tests/battle.rs` — problem-details leak checks, the header-only idempotency
  regression test, Origin/content-type edge cases, 408/413, authorization-before-existence,
  resident persistence, per-event WebSocket authorization, build-time option validation, and
  Inspector token precedence.

No existing test was modified or weakened; the crate's `src/` was not touched (test-only pass).

## Findings

### HIGH-1 — a body `idempotency_key` is trusted as the idempotency key (Python refuses it with 422)

**Status: FIXED in `f11e62e`.** `body.rs` now carries `ADAPTER_OWNED_PAYLOAD_KEYS =
["idempotency_key"]` and `refuse_reserved_send_keys` refuses it with 422; a body `id` stays
allowed. The regression test in `tests/battle.rs` passes, and the full crate suite is 73 tests,
0 failures (`test(axum)` commit `14f8335`).

**Status: OPEN (test-only pass; fix described below, `src/` not edited).** The Rust port omitted
Python's `ADAPTER_OWNED_PAYLOAD_KEYS`, so a client can put `idempotency_key` in the JSON body and
the inbox's `default_key` reads it straight out of the event payload — the exact "a body
`idempotency_key` should not be trusted" case the mandate calls out.

- **file:line**
  - `crates/xstate-axum/src/body.rs:20` — `RESERVED_SEND_KEYS = ["wait", "priority"]`; `:48-53` —
    `refuse_reserved_send_keys` checks only those two, so a body `idempotency_key` passes.
  - `crates/xstate-axum/src/registry/cycle.rs:220` — the only call site; `:229-231` — the
    validated header key is inserted into the payload *after* the check, so reserving
    `idempotency_key` would not break the header path.
  - Python reference: `contrib/starlette/_http.py:256` (`ADAPTER_OWNED_PAYLOAD_KEYS = ("idempotency_key",)`)
    and `:262-270` (`refuse_reserved_send_keys` also rejects the adapter-owned key). The Python
    docstring is explicit: `idempotency_key` "is set only from the validated `Idempotency-Key`
    header: a body that carried one skipped the header's length / charset check and the 501
    'no inbox' refusal".

- **reproduction**
  ```
  CARGO_TARGET_DIR=C:\Users\basil\Desktop\Projects\RustProjects\qa-target-axum \
    cargo test -p xstate-axum --all-features --test battle body_idempotency_key_is_refused_like_python
  ```
  The test POSTs `{"idempotency_key":"k1"}` to `/order/o1/events/NOTE` (inbox + principal
  resolver, no `Idempotency-Key` header).

- **observed vs expected** — observed: **200**; the body key is read by `default_key` and the
  second identical request returns `duplicate: true` (deduplicated) even though no header was
  ever sent. Expected (Python parity): **422** with `error: "ReservedKeyError"`; only the
  validated `Idempotency-Key` header may set this field.

- **impact / severity rationale** — not a cross-tenant security hole (the inbox scope is still
  per principal), but it is wrong behaviour against the documented "header only" contract, it
  silently turns an ordinary payload field into routing state (in-flight 409 / mismatch 422 /
  duplicate 200), and it diverges from Python 0.12.1. Rated HIGH (wrong behaviour / false claim,
  exactly the item the handover pins).

- **proposed fix** (deferred: test-only pass) — mirror Python: keep `RESERVED_SEND_KEYS`
  (`wait`/`priority`) and add a separate adapter-owned set containing `idempotency_key` that
  `refuse_reserved_send_keys` also rejects. The header value is inserted *after* the check
  (`cycle.rs:229-231`), so the header path is unaffected. A body `id` is deliberately **not**
  refused — it is ordinary event data and, as in Python, only falls back to `default_key` when no
  header key was stamped.

## Verified sound

Exercised and passing on this pass (2026-11-07, Windows 11, Rust toolchain pinned), with the one
HIGH-1 regression test as the only failure:

- **Problem details (RFC 9457)** — `type: about:blank`, fixed `title`, `status`, and the Python
  class name in `error` for every `ApiError` variant; the 5xx (`Internal`, `Store::Unavailable` /
  `Corrupt` / `SnapshotTooLarge`) bodies carry **no** `detail` and no store/engine message, so
  paths, keys, payload fragments and credentials never leak; `content-type:
  application/problem+json` on every problem response (battle `problem_details_are_fixed_shape_and_never_leak_internals`,
  plus the existing `problem.rs` unit tests). Note (not a defect): the direct `problem()` refusals
  (cross-origin 403, stream 429/503, probe 503) omit the `error` member, so their shape differs
  from the authorizer 403 (`error: "ForbiddenError"`) — still valid RFC 9457, but worth unifying
  if a client relies on `error`.
- **Idempotency (header path)** — replay returns the cached receipt with `duplicate: true` without
  re-saving; fingerprint mismatch → 422; in-flight → 409; scope is per principal + machine +
  instance; anonymous/placeholder principal → 401; over-long key → 400; 501 without an inbox; the
  255/256-character header boundary is exact (`battle idempotency_key_header_length_boundary` and
  the existing `http.rs` cases). The header-only contract is the one gap (HIGH-1).
- **Origin / Host rules** — same-origin compared case-insensitively (`HTTP://127.0.0.1` vs
  `Host: 127.0.0.1` admitted), allow-listed origin admitted regardless of Host, cross-origin
  refused 403 (POST) / 1008 (WS) / 403 (SSE), missing `Origin` (non-browser) admitted, POSTs
  require `application/json` even with an empty body (415 otherwise), `application/json;
  charset=utf-8` and `Application/JSON` accepted, `text/plain` → 415 (`battle
  origin_rules_mixed_case_allow_list_and_missing`, `post_content_type_enforcement`, plus
  `streaming.rs`/`http.rs`).
- **Body limits / timeouts** — 413 for a `Content-Length` over `max_body_bytes` and for an
  oversized chunk mid-stream; 408 from `read_body` when the body stalls past `body_timeout`
  (`battle read_body_answers_408_on_a_stalled_body_and_413_on_an_oversized_chunk`); `max_body_bytes(0)`,
  `heartbeat(0)`, `body_timeout(0)`, `max_connections_per_key(0)`, `max_connections(0)`,
  `run_timers(0)` and inbox-without-principal all refused at build time (`battle
  invalid_numeric_options_are_rejected_at_build_time`, plus `http.rs`). Routes read the real body
  (`bounded_json`) rather than trusting `Content-Length`; the public `json_body` helper's
  documented `Content-Length: 0` shortcut is pinned by `battle
  json_body_trusts_a_declared_zero_content_length`.
- **Authorization before existence** — an unknown key under a forbidding authorizer answers 403,
  never 404, on both the read and the stream routes (`battle
  authorization_runs_before_the_existence_check`); unknown machine is a 404 (`ApiError`) and a
  `RegistryError::UnknownMachine` when building the router (`battle
  unknown_machine_router_is_a_build_error`); over-long instance key → 400 `InvalidKeyError`.
- **WebSocket** — per-event authorizer runs on every frame (refused event closes with 1008, allowed
  event returns a receipt, `battle websocket_runs_the_authorizer_per_event`); per-event *gates* and
  undeclared events refused as error frames (existing `http.rs`); oversized frame → 1009; malformed
  / bad-shape frames → 400 / 422 error frames while the session stays open; cross-origin → 1008;
  connection caps → 1013 (existing `streaming.rs`).
- **SSE / fan-out** — `snapshot` on connect then `transition` frames with `id:` sequence and
  heartbeat comments; per-key and registry-wide caps → 429; shutdown ends streams; slow-consumer
  cut-off at `MAX_BACKLOG` (existing `streaming.rs` + `fanout.rs` unit tests). The
  subscribe-before-snapshot ordering is implemented in `routes/stream.rs:88-90` and
  `routes/ws.rs:146-147` (subscribe, then peek) — verified by inspection; the race itself is not
  deterministically reproducible from a black-box client.
- **Registry lifecycle** — start/stop/shutdown with graceful drain (open streams end, stuck
  sessions aborted after the drain timeout, then 503 for new streams/sends while reads stay 200);
  resident state survives a brand-new registry over the same store (a read of an unknown key
  reports the initial state without saving; `battle resident_state_survives_a_new_registry_over_the_same_store`);
  durable-timer scanner wakes persisted deadlines and stops on shutdown (existing `lifecycle.rs`).
- **Inspector** — refuses to mount without `debug: true`; token must be `A-Za-z0-9-._~`; loopback
  Host only unless `allow_remote` (421 otherwise); cross-origin refused 403; `?token=` redeem
  issues the `HttpOnly; SameSite=strict` cookie (plus `Secure` when remote) and 303 → path; token
  presented as `Authorization: Bearer` beats `X-XSM-Token` beats the cookie; `Basic` and wrong
  tokens → 401; WebSocket without a token closes 1008 (`battle
  inspector_token_precedence_and_rejection` + existing `lifecycle.rs`).

## Commands run

All with `CARGO_TARGET_DIR=C:\Users\basil\Desktop\Projects\RustProjects\qa-target-axum`.

| Command | Result |
|---|---|
| `cargo test -p xstate-axum --all-features --test battle` | 13 pass, **1 fail** (HIGH-1 regression, intentional) |
| `cargo test -p xstate-axum --all-features -j 4 --no-fail-fast` | lib unittests 27 · battle 13+1 · http 18 · lifecycle 6 · streaming 6 · doctests 2 — the HIGH-1 regression test is the **only** failure; every pre-existing test passes |

No existing test was weakened; the single failure is the new regression test that pins HIGH-1.

## Not tested / why

- **Subscribe-before-snapshot ordering race** and the **WebSocket send-timeout (1011)** and
  **SSE slow-client cut-off** paths were verified by code inspection plus the `fanout.rs` unit
  tests rather than wire-level: the ordering race needs an intercept between subscribe and peek,
  and the send-timeout needs a client that stops reading without closing (the raw TCP helper reads
  every frame).
- **408 at the HTTP route level** was exercised through the public `read_body` helper directly
  (a stalled body stream); a wire-level stalled-body test would be equivalent but slower and
  flakier.
- **Load testing** (`oha`/`wrk`, 1000-subscriber fan-out, p99/memory) was out of scope for this
  pass — no load tooling was set up, and it belongs to the §6.3 non-functional section.
- **Real-browser check** of the Stately inspector iframe / `SseSink --open` is out of scope here
  (that is the `xstate-inspect` §6.2 matrix).
