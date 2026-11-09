# §6.2 Battle test — xstate-persistence

Scope: the §6.2 `xstate-persistence` matrix in `docs/HANDOVER-QA.md` — every public part of the
crate, the `StateStore` contract, FileStore on Windows, durable timers, the idempotency inbox, and
cross-language parity with Python 0.12.1 for SQLite/Redis/PostgreSQL.

Severity counts: **0 CRITICAL · 0 HIGH · 1 MEDIUM · 0 LOW** (one robustness defect found; everything
else in the matrix exercised and sound — see "Verified sound").

New test files (kept as added coverage; the one that found the bug is marked):

- `crates/xstate-persistence/tests/battle.rs` — contract stress (50 writers, lock release on panic),
  FileStore Windows edges, durable timers from a second instance, `RestartTimers` matrix, inbox
  scope/concurrency, and the SQLite `list_keys` regression test.
- `crates/xstate-persistence/tests/battle_postgres.rs` — PostgreSQL `list_keys` regression test and a
  50-writer optimistic-creation check.
- `crates/xstate-persistence/tests/cross_language_battle.rs` — large-snapshot, unicode-key and
  version-race cross-language tests for `FileStore` and `SqliteStore`.

No existing test was modified or weakened; the crate's `src/` was not touched.

## Findings

### MEDIUM-1 — `list_keys` applies its LIMIT before discarding invalid keys in `SqliteStore` and `PostgresStore`

**Status: FIXED in `d8abec6`.** Both queries now filter empty, over-long (and, in SQLite,
NUL-bearing) keys in SQL before `LIMIT`, matching the Redis store; the two regression tests pass
and the full suite is 384 pass / 0 fail with the services up.

- **file:line**
  - `crates/xstate-persistence/src/sqlite_store.rs:737` (SQL `ORDER BY key LIMIT ?3`) and `:747`
    (`keys.retain(validate_key)`) — the retain runs *after* SQLite already truncated to `limit`.
  - `crates/xstate-persistence/src/postgres_backend/ops.rs:119` (`... LIMIT $2`) and
    `crates/xstate-persistence/src/postgres_backend/store.rs:274` (`keys.retain(validate_key)`).
  - `RedisStore` is correct: `src/redis_backend/store.rs:349` filters (`found.retain(...)`) before
    `live_keys` applies the limit.

- **reproduction**
  ```
  cargo test -p xstate-persistence --features "sqlite,redis,postgres" \
      --test battle sqlite_list_keys_filters_invalid_keys_before_applying_the_limit
  cargo test -p xstate-persistence --features "sqlite,redis,postgres" \
      --test battle_postgres list_keys_filters_invalid_keys_before_applying_the_limit
  ```
  (both need `XSM_REQUIRE_SERVICES=1` for the PostgreSQL one; Docker xsm-pg is up)

- **observed vs expected** — a foreign writer plants a row whose key the store itself would refuse
  (an empty string, which sorts before every valid key). With one valid key `"a"` also present,
  `store.list_keys("", 1)` returns `[]` (observed) instead of `["a"]` (expected). The planted key
  consumes the single LIMIT slot, then `retain` removes it, leaving nothing — a valid key is
  starved. The same store's own `save` validates keys, so this needs a foreign/corrupted writer
  (or a Python/legacy writer writing an unusual key), but it contradicts the documented guarantee
  ("a key `load()` would refuse is never advertised") that the store already implements elsewhere,
  and it is exactly the case `RedisStore` gets right.

- **related, lower impact** — `due_keys` in the same two stores
  (`sqlite_store.rs:780-793`, `postgres_backend/store.rs:333-337`) returns keys with no
  `validate_key` filter at all. A planted invalid key with a deadline row surfaces as a per-key
  scanner error rather than being skipped; the scanner handles it without panicking, so this is
  not a separate defect, but the fix below should cover both read-back paths for consistency.

- **proposed fix** (deferred: test-only pass, `src/` not edited) — filter before truncating: fetch
  the matching keys (or a bounded superset such as the prefix matches), `retain(validate_key)`,
  then `truncate(limit)` — the shape `RedisStore` already uses. Alternatively add a SQL predicate
  that excludes empty, NUL-bearing and over-`MAX_KEY_LEN` keys so the `LIMIT` never sees them.
  Either is a ~2-line change per store; both `list_keys` and (for parity) `due_keys` should reject
  or skip invalid keys before any `LIMIT`.

## Verified sound

Exercised and passing on this pass (2026-11-07, Windows 11, Rust toolchain pinned, Docker services
xsm-redis/xsm-pg up, Python 0.12.1 venv):

- **`StateStore` contract, every built-in store** (memory/file/sqlite via the `conformance!` macro
  in `stores.rs`, plus redis/postgres in their own suites): key validation (empty/NUL/200-code-point
  boundary), size caps on save *and* load, versioning (first save = 1, per-key, restart after
  delete), `expected_version` conflicts, `list_keys` literal-prefix + code-point ordering + limit,
  `due_keys` earliest-first ordering/ties/limit, `forget` completeness, corruption (truncated and
  garbled records, bad deadline rows, foreign columns), schema-version refusal (newer refused,
  v1→v2 upgrade), adoption (`from_state_ids`), migration paths.
- **Concurrency**: 50 pessimistic `persisted` writers (no lost update, bodies never overlapped);
  50 optimistic `expected_version=0` creators (exactly one winner) for memory and PostgreSQL;
  concurrent increments / `append_next` / inbox claims across threads and processes (existing suites).
- **Locks**: timeout with holder attribution, release on **drop** and on **panic** (memory/file/
  SQLite each release after the holder thread unwinds), reentrancy/join-on-same-thread (SQLite),
  lease TTL expiry and server-clock measurement (Postgres), only-the-owner-releases (Redis/Postgres),
  cross-instance FileStore exclusion via the OS advisory lock, lock stamp (byte-0 locked byte +
  `{pid} {ts}` holder stamp) on FileStore.
- **FileStore on Windows**: reserved device names (`con`, `nul`, `com1`..`com9`, `lpt1`..`lpt9`,
  `aux`, `prn`, plus `con.txt`/`nul.txt`) all round-trip under `%5f`-prefixed stems; long keys hash
  at the 200-character *encoded* boundary and round-trip; path traversal (`/`, `\`, `..`, `.`,
  absolute and UNC paths) refused while `a:b`, `...`, `a..b`, `.hidden` stay legal; record layout is
  byte-identical to Python; orphan-temp sweep; no temp files left after a conflict/size failure.
- **Durable timers**: arm → drop interpreter → scan and fire from a **second store instance** for
  both `FileStore` (fallback load-every-record path) and `SqliteStore` (indexed path); clock skew
  tolerance; stale/locked/unmapped/failing-key handling; `RestartTimers::{FireDue, Parked, Resume}`
  applied through the public `persisted` wrapper; `run_until` prompt-stop.
- **Idempotency inbox**: 50 concurrent claims → exactly one winner (memory + sqlite); scope escaping
  of `/` and `%` keeps `a/m + k` vs `a + m/k` apart; fingerprint goldens; TTL expiry; fingerprint
  mismatch; duplicates across restarts (sqlite); malformed cached receipt → conservative duplicate;
  `InboxErrorPolicy::{Refuse, Admit}`; the processed ring and crash repair.
- **Cross-language (Python 0.12.1)** — SQLite/Redis/PostgreSQL: each runtime writes, the other
  reads, versions continue, locks exclude each other, inbox/log/deadlines read both ways (existing
  `cross_language.rs`, `redis/cross_language.rs`, `postgres/cross_language.rs`), plus the new
  large-snapshot (~400 KB), exotic-unicode-key and explicit stale-version-conflict cases.

## Commands run

All with `CARGO_TARGET_DIR=C:\Users\basil\Desktop\Projects\RustProjects\qa-target-persistence`
(and `XSM_REQUIRE_SERVICES=1` where services are needed; Python auto-detected via `XSM_PYTHON`).

| Command | Result |
|---|---|
| `cargo test -p xstate-persistence --features "sqlite,redis,postgres" --test battle` | 14 pass, **1 fail** (MEDIUM-1 regression test, intentional) |
| `cargo test -p xstate-persistence --features "sqlite,redis,postgres" --test battle_postgres` | 1 pass, **1 fail** (MEDIUM-1 regression test, intentional) |
| `cargo test -p xstate-persistence --features "sqlite,redis,postgres" --test cross_language_battle` | 3 pass |
| `cargo test -p xstate-persistence --features "sqlite,redis,postgres" --no-fail-fast` (full suite, `XSM_REQUIRE_SERVICES=1`) | **382 pass, 2 fail** — the two MEDIUM-1 regression tests are the *only* failures; every pre-existing test and the rest of the new coverage passes |

Full-suite breakdown (per binary, `--no-fail-fast`): lib unittests 17 · adopt 16 · battle 14+1 ·
battle_postgres 1+1 · cross_language 4 · cross_language_battle 3 · idempotency 33 · log 26 ·
migration 16 · persisted 37 · postgres 39 · receipts 12 · redis 31 · scanner 22 · scanner_timing 1 ·
store_layouts 39 · stores 71. No existing test was weakened; the two failures are the new
regression tests that pin MEDIUM-1.

## Not tested / why

- **Redis/PostgreSQL cross-language was exercised via the existing suites**, not extended with new
  cases in `cross_language_battle.rs` (that file covers `FileStore`/`SqliteStore` only); the Redis
  and PostgreSQL interop files already cover locks, deadlines, inbox and log in both directions.
- **No `cargo fuzz`** (the parser lives in `xstate-core`, out of scope here); corruption was
  exercised by planting/truncating records directly.
- **SMB/NFS advisory-lock behaviour** is documented as unsupported and was not simulated.
