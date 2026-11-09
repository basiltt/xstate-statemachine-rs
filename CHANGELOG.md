# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Every crate of the
workspace shares one version number. The minimum supported Rust version (MSRV) is 1.88 and is
raised only in a minor release.

## [Unreleased]

### Added

- Comprehensive architecture and competitor comparison guide (`docs/book/src/guide/comparisons.md`)
  benchmarking `xstate-statemachine-rs` against Rust FSMs (`statig`, `smlang`), Python (`python-statemachine`,
  `transitions`), JavaScript (`XState v5`), and distributed orchestrators (`Temporal`, `AWS Step Functions`).
- End-to-end Cookbook and Example Gallery (`docs/book/src/cookbook/`) containing eight production-ready
  scenarios with statechart topologies, full copy-pasteable Rust implementations, and verification tests:
  traffic light controller, hierarchical order fulfillment, circuit breaker with exponential backoff and jitter,
  distributed e-commerce saga with automated compensation rollbacks, durable SQLite/PostgreSQL persistence
  with crash recovery, transactional outbox CloudEvents messaging over brokers, Axum HTTP service with live
  Stately Studio inspection, and LLM agent tool-calling loops with budget guardrails and human approval.
- Top-level `README.md` redesigned with hero SVG visuals, mobile responsive presentation, complete competitor
  matrix, verified nanosecond benchmarks (S1–S8), and visual Stately statechart diagrams.
- Configured `.cargo/audit.toml` to align cargo-audit advisory ignores with `deny.toml`.
- Updated `LICENSE` copyright notice to 2026.

### Changed

- Battle-test suites now cover every remaining crate in the workspace (§6.2):
  - `xstate-eda`: CloudEvents 1.0 attribute validation and depth/size bounds, dispatcher failure
    modes, transactional outbox leases under concurrent relays, and 4 live broker adapters
    (Redis Streams, NATS JetStream, RabbitMQ, AWS SQS on ElasticMQ) with bidirectional Python
    interop (230 tests pass).
  - `xstate-patterns` and `xstate-agents`: retry jitter distributions over 10,000 samples, 100-thread
    circuit breaker contention, 20-step sagas with tokio step timeouts, prompt injection immunity,
    secrets masking in context and trace, and tool output truncation (153 tests pass).
  - `xstate-macros`, `xstate-inspect`, and `xstate-observability`: 27 UI compile-fail diagnostic
    tests, clippy pedantic clean macro output, deep/wide charts, deterministic SSE fuzzing, 128-connection
    cap enforcement, and Prometheus 0.12.1 metric/label parity (131 tests pass).
- Performance baselines were re-measured on the same quiet machine with the Python 0.12.1
  oracle: Python S1–S7 are 18.69 / 11.42 / 36.06 / 47.66 / 115 / 45.2 / 190 µs per operation and
  the Rust p50 is 106 / 161 / 215 / 283 / 4.11 µs / 300 ns / 3.39 µs. S1, S3, S4 and S6 are past
  100x Python (177x, 168x, 169x, 151x), S2 is 71x, S7 56x and S5 28x. The README tables,
  `benches/README.md` and `benches/budgets.json` (`python_per_op_ns`, baselines and 100x
  targets) now carry these values; the retracted 0.12.0 numbers are gone.
- `xstate-tokio`'s test suite now carries the §6.2 actor matrix: paused-clock timers and
  cancellation, every mailbox overflow policy, sends to finished/stopped actors, child and
  invoke lifecycles, `systemId` collisions, panic containment, restart/drain and observers (32
  tests pass).
- `xstate-axum`'s test suite now carries the §6.2 HTTP battle matrix: problem-details leak
  checks, Origin/content-type edges, 408/413, authorization-before-existence, resident
  persistence, per-event WebSocket authorization, build-time option validation and inspector
  token precedence (73 tests pass).
- The documentation site builds: `docs/book/` is now a 35-page mdBook folding in the existing
  guides, ADRs, crate READMEs, CONTRIBUTING and CHANGELOG, with a stdlib link checker
  (`scripts/linkcheck.py`) and a `pages.yml` workflow that publishes the book plus rustdoc under
  `/api/`. It has never run (the GitHub remote does not exist yet).
- `xstate-core` gains a `fast-path` cargo feature (on by default) gating the single-transition
  fast path; `xstate-conformance` forwards it and CI now runs
  `cargo test -p xstate-conformance --no-default-features`, which exercises every conformance
  case and golden on the general microstep.

- `xstate-persistence`'s test suite now carries the §6.2 battle matrix: 50 pessimistic and 50
  optimistic concurrent writers, lock release on panic, durable timers fired from a second
  store instance, every `RestartTimers` variant, FileStore Windows path edges, 50-way inbox
  claims, scope escaping, and ~400 KB / unicode / stale-version cross-language cases against
  Python 0.12.1 (384 tests with the Docker services up).

- `xstate-core` now pins `maxIterations` behaviour in tests: a self-raising `always` chain stops
  with `Error::RunawayChain` (and a failed `start()` ends stopped), while a context-changing
  `always` that settles within the budget runs to completion.

- `xstate-core` now fuzzes its parser in-process: proptest feeds arbitrary JSON values and
  arbitrary text through `MachineConfig::from_value`, `Machine::from_value`,
  `Machine::from_json` and `Snapshot::from_json` and asserts they never panic (2000 cases per
  run, in CI).

- `xstate-core`'s test suite now carries the 17-machine fast-path adversarial matrix and the
  34 adversarial direct-compiler documents from the QA audit, so they run in CI instead of
  living in a scratch copy.

- `xstate-persistence`: `FileStore` no longer rewrites a lock file's holder stamp on every
  acquisition. The stamp (`<pid> <wall time>`, from byte 1 of the `.lock` file) is read first and
  left alone when it already names the acquiring process and is younger than `stale_lock_after / 2`,
  so the common case — one process taking its own lock again and again — skips a write that an
  on-access antivirus rescans when the handle closes (matching the Python library's
  `FileStore._stamp_is_fresh`; the OS lock stays the source of truth). A stamp by another pid, an
  unparseable one, an older one or one dated in the future is rewritten as before (the Python
  side released this change as 0.12.1 on 2026-11-07).

- Conformance fixtures were regenerated against Python 0.12.1 (released 2026-11-07), and CI now
  pins the Python oracle to `xstate-statemachine==0.12.1` instead of installing `main`. The
  regeneration changed only the recorded Python version string in the 53 case files — zero
  behavioural differences (53 cases, 0 divergent, 0 stale).

### Added

- `xstate-core`: `benches/python_parity.rs` mirrors the Python library's cross-library benchmark
  (scenarios S1 to S7 of `benchmarks/competitors/`, 0.12.0 sync engine) with the same machines,
  logic and per-operation work; the ids (`py_s1_flat_toggle` ... `py_s7_delayed_transition`) are
  in `benches/budgets.json` with the Python time per operation (`python_per_op_ns`, a new
  optional field of `budgets.schema.json`) and a 100x target.
- `scripts/perf_gate.py` (standard library only): runs the engine benchmarks (or reads
  `target/criterion` with `--no-run`), compares each median with
  `baseline_p50_ns x allowed_slowdown`, prints the speed-up over Python, and exits 1 on a
  regression; `--relative` divides out the machine speed factor, `--advisory` never fails,
  `--update-baselines` records the medians. CI's `bench-gate` job now runs it with
  `--advisory --relative` (hosted runners are too noisy to gate on).

### Fixed

- `xstate-eda`: a panicking `DeadLetterSink::put` inside `InboundDispatcher::handle` is now
  caught and mapped to `Flow::Requeue` infrastructure failure rather than escaping the dispatcher
  and crashing the caller task.
- `xstate-persistence`: PostgreSQL test schema creation in `battle_postgres` uses an atomic
  counter to prevent schema name collision across concurrent test threads.
- Repository hygiene: updated `async-executor` from yanked `v1.14.0` to `v1.14.1`, ignored
  `libsqlite3-sys` advisory `RUSTSEC-2026-0459` in `deny.toml` (requires Rust 1.89 via rusqlite
  0.41, exceeding MSRV 1.88; local crafted database attack vector), and dropped unused `MPL-2.0` allowance.
- `xstate-tokio`: a panic in an action, guard, timer effect or service no longer kills the
  actor task silently. The panic is contained: the actor enters `Status::Error` with the panic
  message and tears its children/services down, and a panicking async service reaches the parent
  as `error.actor` instead of hanging the invoke. `send_and_wait` now applies the overflow
  policy (`Reject` → `LimitExceeded`, `DropNewest` → drop with `LimitExceeded`) instead of always
  blocking. `ActiveState::fail` is the new core helper for drivers that contain a panic.
- `xstate-axum`: a request body carrying `idempotency_key` is refused with 422, matching
  Python's adapter-owned payload keys. It previously reached the idempotency inbox and could
  deduplicate a request without the validated `Idempotency-Key` header (bypassing its length and
  charset checks). A body `id` remains ordinary event data.
- Documentation: the migration guide records that `MachineLogic::validate` reports the first
  missing implementation (Python lists every name), and maps the Python-only utilities
  (`has_dormant_timers`, `Clock.pump_filtered`, `SQLiteStore.close`, `register_global`) to their
  Rust structural answers.
- Documentation: the persistence parity mapping now names the real counterparts
  (`IdempotencyGuard` is the Python `IdempotencyPlugin`'s decision logic; adoption is
  `adopt::from_state_ids`, not an `adopt` symbol), the coverage claim is limited to the static
  targets that exist (there is no runtime collector), and the README parity table targets
  Python 0.12.1.
- Documentation: the migration guide no longer claims that `invoke` `input` / `output` factories
  exist — they are a recorded parity gap (`invoke.input` is static JSON; Python 0.12.0's
  event-driven input factory and `output` factories are not ported) — and its engine-semantics
  section now records that the XState v5 convergence shipped in Python 0.12.0 rather than an
  unreleased branch. The port target is stated as Python 0.12.1.
- `xstate-axum`: the same-origin check compares the `Origin` host with `Host`
  case-insensitively, matching Python's shared `origin_is_allowed` rule; a mixed-case `Host`
  header was a false refusal. Allow-listed origins still match exactly.
- `xstate-eda`: a settle (`ack` / `drop_message`) that the transport fails now leaves the
  delivery in flight, so the caller can retry it with the same `Delivery` instead of getting a
  silent no-op; and the RabbitMQ transport releases its un-acked slot even when the broker
  rejects the settle, so a failed ack can no longer shrink `prefetch` until `fetch` stalls.
- `xstate-core` / `xstate-persistence`: redaction now matches Python 0.12.0 exactly. The core
  denylist gained `oauth`, `creditcard` and `cardnumber` (they were leaking through the
  inspector, dead-letter and outbox redaction), and the transition-log observer no longer keeps
  its own pre-0.12.0 copy of the rule — it delegates to `xstate_core::redact::RedactKeys` — so
  ordinary keys such as `shipping`, `company`, `author` and `spinner` are no longer over-redacted.
- `xstate-persistence`: `list_keys` on the SQLite and PostgreSQL stores now drops keys that
  `load()` would refuse *before* applying `LIMIT`, matching the Redis store. A foreign writer
  that planted an empty key (which sorts first) used to consume the limit slot, so
  `list_keys(_, 1)` could return no valid key at all.
- `SECURITY.md` now names a working private-reporting channel (GitHub's private vulnerability
  reporting) instead of an author email the Cargo manifest does not contain, and links a real
  threat-model page (`docs/book/src/operations/security.md`) covering the parser, snapshot
  restore, EDA envelopes, the SSE inspector and the axum surfaces.
- `conformance/tools/agents_golden.py` writes its golden files with `\n` line endings, so
  regenerating them on Windows no longer leaves the working tree dirty.
- `xstate-core`: a machine document nested deeper than 128 state levels (root counted as 0)
  is refused with `Error::LimitExceeded` instead of overflowing the thread stack (an
  uncatchable process abort) when a programmatically built `Value` is compiled; both the
  direct compiler and the parser enforce the bound, and the direct compiler falls back to the
  parser's typed error (serde_json's own 128-level cap already protects JSON text).
- `xstate-core`: `Machine::to_json` / `to_json_string` no longer return `null` for a source
  document nested past serde_json's 128-level reparse limit. The stored text was written by
  this process, so it is reparsed with the recursion limit disabled (JSON text from callers
  keeps the default limit).
- `xstate-core`: a state `type` outside `atomic`/`compound`/`parallel`/`final`/`history`
  (`"Final"`, `"paralel"`) and a `compound` / `parallel` state without child states are
  `InvalidConfig` errors naming the state and value; an `atomic` / `final` / `history` state
  that declares children is built as compound with a `ConfigWarning` (a `final` one used to be an
  error, a `compound` one without children silently became atomic).
- `xstate-core`: a history state's default `target` must be a non-empty string; an invalid
  `history` value names the state.
- `xstate-core`: `strict`, `strictTargets`, `strictConfig` and the transition flags `reenter` /
  `internal` accept only `true`, `false` or `null` ("not set"); a non-boolean `strictConfig` was
  silently ignored and `null` was refused.
- `xstate-core`: configuration errors from action, guard and transition parsing name the state
  and key (`State 'm.a' entry: ...`, `Transition 'GO' on state 'm.a': invalid 'guard': ...`,
  `State 'm.a' on 'GO': invalid transition item ...`).
- `xstate-core`: action `params` must be an object (or `null`) for every action, not only
  built-ins. Unknown keys of action objects, of guard objects and of a built-in's `params`
  (`"delya"`) are warnings, or errors under `strictConfig` (an unknown action-object key was
  always an error); a built-in written flat (`{"type": "raise", "event": ...}`) is refused with a
  hint to nest its parameters under `params`.
- `xstate-core`: `maxIterations` accepts an integral float (`5.0`) and still refuses `0`,
  negatives, `2.9`, `true` and `null`; `spawnBlockingTimeout` must be a finite number `>= 0`.
- `xstate-core`: an `initial` naming no child is refused at parse time with the list of children
  and a "did you mean" hint, as XState's `createMachine` does.
- `xstate-core`: `after` keys must be whole, non-negative milliseconds or a delay name. Numeric
  strings are normalised (`"1e3"` is 1000 ms, `" 100"` is 100 ms) and the timer event, the
  structure hash, snapshots and diagrams use the normalised value (`xstate.after.1000.<id>`, as
  XState and Python do); `"1.5"`, `"-5"` and an empty key are refused, and two keys naming the
  same delay are an error. `"inf"` / `"NaN"` are delay names, no longer numbers.
  `AfterDef::raw_key` keeps the key as written. Pinned by conformance case 053.
- `xstate-core`: invoke `id` / `systemId` must be non-empty strings and an invoke `description`
  a string; an empty-string `target` is refused at parse time with the state named.
- `xstate-core`: a user action named like a built-in (`log`, `cancel`, `stop`, `emit`, ...)
  wins over the built-in: when the logic implements it, the engine runs the user action, and
  the new `Machine::from_value_with_logic` exempts such names from the built-in's `params`
  check. `MachineLogic::action_names` lists the registered actions; `MachineLogic::validate`
  now also checks named delays of delayed `raise` / `sendTo`.
- `xstate-core`: dotted ids resolve: a custom state id `"my.id"` can be targeted as `#my.id` and
  `#my.id.child` (longest dotted prefix), and `Machine::state_by_id` accepts the same forms.
- `xstate-core`: restoring a snapshot whose pending or deferred event record has an unknown
  `kind`, a non-object `payload`, no `type` or a non-numeric timer timestamp fails with
  `Error::Snapshot` instead of restoring it as user traffic; `done` / `error` / `after` records
  marked `"engine": true` restore as engine events.
- `xstate-core`: an action failing inside `choose` follows `actionErrorPolicy` exactly like a
  top-level one (under `continue` the rest of the enclosing list is skipped as well), and the
  `TransitionFailed` error names the innermost failing action instead of wrapping it twice.
- `xstate-core`: a delay that resolves to NaN or infinity, or whose delay function fails,
  disables that one timer with an error log on every path (`after`, delayed `raise`, delayed
  `sendTo`) instead of failing the step; an unregistered delay name is still
  `ImplementationMissing`.
- `xstate-core`: a JSON `assign` on a context that is not an object (an array, a number) is an
  action error handled by `actionErrorPolicy` instead of silently replacing the context with
  `{}`.
- `xstate-core`: `sendTo` / `forwardTo` with no live target and `sendParent` / `escalate`
  without a parent drop the event with `on_event_dropped("unresolved_target")` and mark the step
  failed: the transition commits, `Receipt::error` / `StepReport::soft_error` carry the new
  `Error::UnresolvedTarget` (`ErrorKind::UnresolvedTarget`). `SyncInterpreter` has no actor
  system, so every such send is unresolved there; a delayed `sendTo` to `self` is now a timer of
  the machine instead of being dropped.
- `xstate-core`: `send("")` and `Event::from_json` with an empty type are `InvalidEvent`.
- `xstate-core`: `SyncInterpreter::stop` reports every queued event with
  `on_event_dropped("stopped")` instead of discarding it.
- `xstate-core`: `SimulatedClock::try_increment` / `try_set` refuse NaN and infinity
  (`increment` / `set` ignore them with an error log) and `SyncInterpreter::advance` returns the
  error; the interpreter module's snapshot and restore code moved to `interpreter/restore.rs`.
- `xstate-core`: Mermaid / PlantUML exports draw every alternative of a state-level `onDone`,
  as Python 0.12.0 does (conformance case 051 pins that every alternative is a candidate).
- `xstate-tokio`: a `sendTo` to a child and a child's stop are put in its mailbox at once, in
  effect order, so an exit action's `sendTo` reaches the state's own invoke before it is stopped
  (XState `exitStates`); they were two racing tasks.
- `xstate-tokio`: unresolved `sendTo` / `sendParent` / `escalate` (delayed ones included) are
  reported like on `SyncInterpreter` (`unresolved_target`, `Receipt::error`); a delayed `sendTo`
  to `self` is a cancellable local timer and a delayed `sendParent` reaches the parent (both were
  dropped); `send("")` is `InvalidEvent`; events still in the mailbox when the actor stops are
  reported as dropped (`stopped`) and their waiters get an error.
- `xstate-core`: `redact` / `RedactKeys` follow the Python 0.12.0 matching rule: keys are
  normalised to `snake_case` (camelCase boundaries split, `-` as `_`, case-insensitive) on both
  sides, and denylist entries of at most four characters (`pin`, `pan`, `otp`, `card`, `auth`,
  `seed`, ...) match only a whole word of the key, so `shipping`, `company`, `author` and
  `spinner` are no longer redacted while `user_pin`, `userPin` and `cardNumber` still are;
  longer entries keep substring matching (`sessionId`).
- `xstate-persistence`: `receipts::STATUS_NO_OP` (HTTP 200, Python's renamed
  `STATUS_NOT_MODIFIED_OK`) names the clean no-op status; `ReceiptErrorKind::UnresolvedTarget`
  (Python's new `UnresolvedTargetError`, a subclass of `ActorSpawningError`) maps to HTTP 500.
- `xstate-persistence`: `ReceiptJson::to_value_with` / `to_json_with(include_messages)` write
  error messages as `""` for receipts shown to untrusted clients; `to_value` / `to_json` keep the
  message.
- `xstate-persistence`: `FileStore::delete` / `forget` take the key's lock, so a concurrent save
  can no longer resurrect a deleted record; `list_keys` only returns keys `load` would accept
  (and fills `limit` with them); a save fsyncs the directory after the rename on Unix.
- `xstate-persistence`: `MemoryStore::forget` no longer discards a lock that is still held or
  awaited, which let a second holder in.
- `xstate-persistence`: `SqliteStoreOptions::busy_timeout` above `i32::MAX` ms is refused at
  open; `JournalMode` offers only `Wal` and `Delete` (the other modes are removed).
- `xstate-persistence`: `PersistedOptions::expected_machine_hash` is checked against the machine
  itself on a new key and against the stored snapshot on a restore, where it also forces the
  drift check on when `verify_hash` is `false`.
- `xstate-persistence`: `DeadlineExt::remaining_ms` clamps to `0..=MAX_REMAINING_MS` (`1e15`)
  instead of returning infinity for a huge finite `due_at_wall`.
- `xstate-persistence`: `DueTimerScanner` gained `validate()` (negative or non-finite
  `skew_tolerance_s`, `limit == 0` are refused instead of clamped; called by every scan),
  `last_result()` (`None` before the first scan) and `run_until_checked`, which rejects a zero
  interval; `run_until` logs and returns instead of spinning.
- `xstate-core`: `start()` landing directly in a top-level final state now runs the exit actions
  of the final state and the root, as XState v5 does (`e_root, e_done, x_done, x_root`).
- `xstate-core`: `always` transitions are selected in the order their atomic states were entered,
  as XState's `selectEventlessTransitions` walks `snapshot._nodes` (states a microstep keeps stay
  in place, the ones it enters are appended in document order; a microstep that re-enters exactly
  what it exited without recording history keeps the order; a restored snapshot starts in
  document order). The engine visited them in document order, so after one region moved, the
  other region's `always` guards ran second and a conflict between two regions' `always`
  transitions could go the other way. `ActiveState` keeps a per-state entry stamp
  (`entered_at`); the eventless pass sorts its (inline) leaf list by it.
- `xstate-core`: an eventless microstep that leaves the snapshot unchanged -- same states, no
  history recorded, no `assign`, no actor started or stopped, no in-place context change by a
  user action -- no longer re-selects eventless transitions (XState's `macrostep`). A
  self-targeting `always` with `reenter: true`, or on a compound state, ran again until
  `maxIterations` failed the machine; it now runs once per macrostep (and once more per internal
  event). `ActiveState::mutations` counts snapshot changes instead of every exit and entry.
- `xstate-core`: a parallel state with a `history` child now completes (`done.state.*`, `onDone`)
  when its regions are final: `isInFinalState` skipped no child, and a history pseudo-state is
  never final.
- `xstate-eda`: `replay_dead_letter` refuses a dead letter whose stored envelope `data` holds the
  redaction sentinel `"***"` as a leaf (it would have replayed masked values and resolved the
  record), with `EdaError::ReplayRefused` unless `ReplayOptions::force` is set (Python 0.12.0).
- `xstate-eda`: a `StoreError` that is an outage (`Unavailable`, `Conflict`, `LockTimeout`,
  `SnapshotTooLarge`, `InvalidKey`) returned by an `EnvelopeHandler` converts to the new
  `HandlerError::Requeue`, which the dispatcher requeues without counting an attempt instead of
  dead-lettering the envelope as poison after `max_attempts`; `Corrupt` and engine errors still
  count (Python 0.12.0).
- `xstate-eda`: `SqliteOutboxStore` logs and skips a row whose envelope no longer decodes instead
  of failing the whole read, and `claim` parks it under the new `POISON_OWNER` lease so no relay
  selects it again; later rows keep relaying (Python 0.12.0).
- `xstate-eda`: `OutboxPlugin` logs a warning when a `meta.publish` `data` field is missing from
  the context (the field is still published as `null`).

### Changed

- `xstate-core`: event dispatch is 2 to 3 times faster and allocation-free (`send_flat` 446 to
  192 ns, `send_nested_3` 642 to 212 ns, `send_parallel_2` 543 to 213 ns, Python-parity S1 to S4
  at 74x, 39x, 80x and 92x the Python library); machine construction is about twice as fast
  (S5 13.6 to 6.9 µs, `build_50_states` 203 to 129 µs). Semantics are unchanged (all conformance
  cases, goldens and tests pass). What changed inside the engine:
  - per-machine precomputed transition domains and entry sets (machines without history
    states), exit sets read from the domain's subtree range, a configuration bitset stored
    inline for up to 128 states, transition selection that walks up from the single active leaf
    of a machine without parallel states and skips subtrees that handle none of the event's
    descriptors;
  - `SyncInterpreter::send` processes the event in place when nothing is queued ahead of it,
    reuses its effect buffer for `start`, reads the clock only when a timer is armed, and builds
    one report per `send`; `settle` is skipped when nothing is eventless or raised;
  - `done.state` events are no longer built when the same entry finishes the machine (they were
    discarded unseen), and `after` timer event types are interned at compile time;
  - parsing reads configuration objects by scanning their few keys instead of hashing, builds
    state ids in one buffer, and `Machine::from_value` / `from_json` keep the source document as
    compact JSON text instead of a deep copy of the value (`Machine::to_json` parses it back:
    the document is equal to the input, at the cost of a parse when it is asked for);
  - `MachineLogic::validate` returns at once for a machine that names no action, guard,
    service or delay, and the logic registry hashes with `foldhash`.
- `xstate-core`: `SyncInterpreter::new` shares one process-wide `RealClock` instead of creating
  one per interpreter (only differences of its readings are used, for timer due times).
- `xstate-core`: no longer depends on `fixedbitset`.
- `xstate-core`: second performance pass. Interleaved with the first pass's tree on the same
  machine (criterion medians, best of three each): S1 -20 %, S2 -24 %, S6 -48 %, S7 -24 %,
  S5 -11 %, `send_nested_3` / `send_parallel_2` -18 % / -20 %, `build_phase_compile` -32 %.
  Semantics are unchanged (53 conformance cases, goldens, workspace tests and the Python
  bindings' tests pass). What changed:
  - **API:** `Receipt::actions` and `StepReport::actions` are now `ActionList`
    (`SmallVec<[Arc<str>; 2]>`, exported from the crate root; derefs to `[Arc<str>]`) instead of
    `Vec<Arc<str>>`, so a send whose step runs one or two user actions allocates nothing.
    `Receipt::action_names()` still returns `Vec<String>`. No caller in the workspace needed a
    change.
  - `Effect::CancelTimers` is emitted only for exited states that declare `after` entries (the
    only states that own timers), so a step that arms no timer emits no effect and an
    interpreter that never arms one never allocates its effect buffer.
    `SyncInterpreter::from_snapshot` now refuses (`Error::Snapshot`) a deadline whose state
    declares no `after` timer; such a timer could never have been armed by the engine.
  - The logic registry stores its names in insertion order and finds them by comparing names
    (a hash index is built above 16 entries): creating an empty `MachineLogic` no longer seeds
    four hash maps and a guard or action lookup no longer hashes its name.
    `MachineLogic::action_names` lists the actions in registration order.
  - The exit set of a single transition is appended in document order without searching, the
    entry set of a planned transition is borrowed from the machine instead of copied, and the
    set of completed parallel states is only created when a final state is entered.
  - `start` builds the runtime state in the interpreter's slot instead of moving it there, and
    skips settling when the machine has no eventless transition and nothing was raised.
  - Compilation finds event keys and child states by scanning for machines with up to 8 event
    types / 32 states (hash indexes above) and keeps the flattened state list inline: three
    fewer allocations for the S3 machine (61 to 58).
- `xstate-core`: third performance pass. Interleaved with the second pass's tree on the same
  machine (criterion medians, best of seven runs each): S1 -37 %, S2 -18 %, S3 -31 %, S4 -34 %,
  S5 -43 %, S6 -19 %, S7 -30 %; S1, S3 and S4 now run at more than 100x the Python library.
  Semantics are unchanged (53 conformance cases, goldens, workspace tests and the Python
  bindings' tests pass). No public API changed. What changed:
  - **Plain microsteps.** Per transition the precomputed plans now also record whether taking
    it alone is *plain*: nothing it can exit has exit actions, `after` timers or invocations,
    and nothing it enters has entry actions, `after` timers or invocations or is final (and the
    machine has no history states). A single enabled plain transition skips the general
    microstep (conflict resolution, exit / entry set construction, history recording,
    completion checks, per-state effect scans): it clears the exit range's configuration bits,
    runs the transition's actions and sets the precomputed entry set, with the same report,
    entry stamps and snapshot-change count. A randomized differential test
    (`engine::fast_path_tests`, `proptest`: random machines of up to three levels with
    parallel regions, finals, history, guards, actions, entry / exit actions, `reenter` and
    guarded `always`, driven by random event sequences) runs every case with the fast path on
    and off and compares receipts, runtime state and action traces.
  - **Direct compilation.** `Machine::from_value` / `from_json` compile plain documents straight
    from the JSON value into the arena (`machine::direct`): the same `Compiler` resolves
    targets, interns events and builds the tables, but no owned `MachineConfig` is built. Plain
    means plain states (`id`, `initial`, `states`, `type`, `on`, `always`, `after`, `entry`,
    `exit`, `description`), transitions as target strings or objects with `target`, `guard`
    and `actions` by name, `reenter` and `description`, user actions only, and a root with at
    most `context` and `version` besides; everything else (built-ins, action / guard objects,
    `invoke`, `onDone`, history, policies, `x-` keys, unknown keys, and every malformed
    document) goes through the parser as before, which produces every warning and error. A
    test builds every machine document of the repository (conformance cases, CLI fixtures,
    agent charts), 79 edge cases and 400 random machines both ways and compares the compiled
    machines field by field (and the errors). The S3 machine now takes 20 allocations instead
    of 58. `MachineConfig` and `Machine::from_config` are unchanged.
  - The initial entry set is computed once per machine (nothing is recorded in history before
    a machine starts); interpreters built with `SyncInterpreter::new` borrow the process-wide
    real clock instead of holding an `Arc` to it; a step that requests no effect skips the
    effect pass; the receipt of a machine without parallel states reads its single leaf off
    the configuration; event types and implementation names are compared inline instead of
    through `memcmp`; `send(&str)` builds short event types inline.

- Conformance: the Python engine converged on XState v5 for the eight recorded divergences
  (Python branch `xstate-v5-engine-parity`); `conformance/divergences.generated.md` is empty and
  cases 020, 021, 027–032 now carry identical Python and XState expectations.
- Conformance: eight new cases (`038`–`045`) pin the remaining engine differences the Python
  engine has now converged on -- conflict preemption and exit/actions/entry grouping in one
  microstep, `done.state.*` on the internal queue, guard evaluation order, `assign` ordering, a
  compound self-target without `reenter`, and a JSON `assign` without the `assignment` wrapper.
  The case format gained a `send_batch` step (events queued together before processing) and
  `logic.record_guards`, which adds the guard evaluations to every observation (`guards`); the
  XState oracle, the Python generator, the schema and `xstate-conformance` (`Step::SendBatch`,
  `Logic::record_guards`, `Observation::guards`, `runner::stub_logic_recording`) support both.
  The Rust engine already matched XState on all eight. The Mermaid / PlantUML goldens now include
  `031_multi_target_parallel_entry`, which the Python library renders since it accepts
  multi-target transitions.
- Conformance: five new cases (`046`–`050`) pin the last known engine differences, now fixed in
  both engines: `always` selection in entry order (`046_always_order_follows_entry_order`,
  `050_always_order_after_reentry`), history recorded only when its owner is exited
  (`047_history_records_only_on_owner_exit`; the Rust engine already did this), a self-targeting
  `always` runs once (`048_always_self_target`) and which ancestors receive `done.state.*`
  (`049_done_state_propagation`: the compound parent, then every completed parallel ancestor; a
  `"*"` handler sees the ones nobody handles).

## [0.1.0] - 2026-11-02

First release: a Rust port of the Python library
[`xstate-statemachine`](https://github.com/basiltt/xstate-statemachine) 0.12.0 with an XState v5
compatible engine, Python-compatible snapshots and store layouts, and the feature set described
below. Developed in phases 0 to 7; the headings follow them.

### Added

#### Phase 0: design and foundations

- Feasibility report and five ADRs: spec-faithful engine semantics with XState v5 as the oracle
  (ADR-0001), a two-tier dynamic / typed context and event model (ADR-0002), byte-compatible
  snapshots and wire formats with the Python library (ADR-0003), a sans-I/O step engine shared by
  every interpreter (ADR-0004) and the workspace and quality gates (ADR-0005).
- Cargo workspace (edition 2024, MSRV 1.88, `unsafe_code = "forbid"`, clippy pedantic,
  `missing_docs`), CI (rustfmt, clippy, tests on three operating systems, MSRV, rustdoc,
  cargo-deny, cargo-audit, coverage, conformance fixture freshness) and the benchmark budget
  layout.
- Conformance suite: JSON fixtures run through two oracles, the real XState 5.33 engine under Node
  and the Python library, recording both observations plus Python's `machine_hash` and snapshot
  vectors. Divergences between the oracles are documented and generated.

#### Phase 1: the engine (`xstate-core`, `xstate-tokio`)

- Lenient XState v5 JSON parser (shorthands, `cond` / `internal` v4 aliases, forbidden
  transitions, inferred `initial`, unknown-key warnings or errors under `strictConfig`, editor
  keys `types` / `schemas` accepted), machine compiler with resolved targets, composite guards and
  every built-in action (`assign`, `raise`, `sendTo`, `sendParent`, `forwardTo`, `escalate`,
  `log`, `cancel`, `stopChild`, `spawnChild`, `emit`, `choose`).
- Sans-I/O step engine transcribed from XState v5 (conflict preemption, exit and entry sets,
  history, `onDone` propagation, eventless settling with `maxIterations`, action and guard error
  policies (`actionErrorPolicy` defaults to `fail`, as in XState), `onUnhandled` ignore / defer / error) and the synchronous interpreter with a timer
  heap and `Clock` / `SimulatedClock`.
- Snapshots in the Python layout v4 (same fields, order and values; compact JSON where Python's
  `get_snapshot()` indents, pinned by a text comparison against Python's output) and the machine
  structure hash, byte-for-byte; restore with hash verification and timer restart modes
  (parked / restart / resume / fire-due; `RestoreOptions::default()` resumes, where Python's bare
  `from_snapshot` parks).
- `xstate-tokio`: `spawn` and the cloneable `Interpreter` handle, bounded mailbox with
  Block / DropNewest / Reject overflow, `after` timers on `tokio::time`, async services, child
  machine actors, `sendTo` / `sendParent` routing, `systemId` registry, watch-based `view()`.
- Builder API, pure API (`initial_transition`, `transition`), exporters (`to_json`, Mermaid and
  PlantUML, byte-identical to Python on the fixture machines), graph utilities (`reachable_states`,
  `shortest_paths`, `simple_paths`, coverage targets, `stub_logic`) and the `Observer` trait.
- Engine benchmarks and the first performance budgets.

#### Phase 2: persistence (`xstate-persistence`)

- `StateStore` contract with memory, file (`.xsm.json` records, atomic rename, advisory locks) and
  SQLite stores sharing the Python layouts; cross-language tests in both directions.
- Optimistic and pessimistic locking, the `persisted` transaction wrapper with retries and
  after-commit callbacks, durable `after` deadlines and `DueTimerScanner`, the idempotency inbox
  and receipts, the transition / audit log, snapshot migrations and `adopt`.

#### Phase 3: observability, inspector and CLI (`xstate-observability`, `xstate-inspect`, `xsm`)

- `MetricsObserver` over the `metrics` facade with the Python `PrometheusPlugin` metric set and
  label hygiene (declared events only, capped cardinality).
- Stately Inspector protocol (`@statelyai/inspect` 0.7.2): `InspectorObserver` with deny-by-default
  context and payload allow-lists and key redaction, memory and JSONL sinks, paced replay;
  recordings are byte-identical to the Python `InspectorPlugin`.
- `xsm` CLI: `validate`, `inspect`, `diagram`, `docs`, `simulate` (scripts, guard overrides,
  virtual clock, `--record`), `paths`, `snapshots`, `info`, with `--json` output and exit codes
  0 / 1 / 2.

#### Phase 4: event-driven architecture and patterns (`xstate-eda`, `xstate-patterns`)

- CloudEvents 1.0 `Envelope` (byte-identical JSON, UUIDv7 ids), the `BrokerAdapter` /
  `SyncBrokerAdapter` contract with fake brokers, `InboundDispatcher` (per-subject order, inbox
  dedup, bounded retries, poison messages to dead letters, replay), the transactional outbox with
  memory and SQLite stores and a relay with claim leases, dead-letter stores and plugin, AsyncAPI
  3.0 generation.
- `RetryPolicy`, `CircuitBreaker`, `SagaBuilder` and `ChoreographyRouter`, emitting the same chart
  JSON as the Python builders.

#### Phase 5: typed DSL, HTTP and Redis (`xstate-macros`, `xstate-axum`)

- `#[derive(TypedEvent)]`, `#[derive(TypedState)]` and the compile-time checked `statechart!`
  DSL, with `trybuild` diagnostics tests.
- `xstate-axum`: `StatechartRegistry`, REST and per-event routes, SSE and WebSocket streaming,
  RFC 9457 problem details, idempotency keys, authorizers, probes, graceful drain and the
  token-gated Stately Inspector WebSocket endpoint.
- Redis backends: `RedisStore`, `RedisInbox`, `RedisLog` (feature `redis` of `xstate-persistence`)
  and `RedisStreamsBroker` (feature `redis-streams` of `xstate-eda`) with the Python key layout
  and Lua compare-and-set scripts.

#### Phase 6: PostgreSQL, more brokers, live inspector, agents (`xstate-agents`)

- PostgreSQL backends over sqlx (feature `postgres`): `PostgresStore`, `PostgresInbox`,
  `PostgresLog` with the Python SQLAlchemy schema, lease-row locks and compare-and-set saves.
- `NatsBroker` (JetStream, durable pull consumers, `Nats-Msg-Id` dedup) and `RabbitMqBroker`
  (durable queues, publisher confirms), async and blocking.
- `SqsBroker` (feature `sqs` of `xstate-eda`, exposed by the facade as `sqs`): the Amazon SQS
  adapter (queue per topic, FIFO groups and deduplication ids, long polling, visibility-timeout
  redelivery), wire-compatible with the Python library's `contrib.brokers.sqs`. Tested against
  ElasticMQ through `XSM_SQS_ENDPOINT`.
- `SseSink`, a standard-library-only hardened HTTP/SSE server for the Stately Inspector page, and
  `xsm inspect --live` / `xsm replay`.
- `xstate-agents`: the tool-loop, supervisor, pipeline and debate charts, `AgentLogic` with
  allow-lists, schema checks and approvals, budgets as guards, tool registry, runner with
  snapshot resume, trace observer, structured output and multi-agent helpers; six Python golden
  scenarios match exactly.

#### Phase 7: release preparation (`xstate-statemachine`)

- The `xstate-statemachine` facade crate: one dependency re-exporting every library crate behind
  feature flags that mirror the Python extras (`tokio`, `persistence`, `sqlite`, `redis`,
  `postgres`, `eda`, `redis-streams`, `nats`, `rabbitmq`, `sqs`, `patterns`, `observability`,
  `inspect`, `macros`, `axum`, `agents`, `testing`, `full`) and a `prelude`.
- README for every crate, a refreshed root README with a feature-parity matrix, and the guide in
  `docs/guide/`: getting started, the Stately editor, persistence, event-driven architecture,
  migrating from Python and the feature-flag reference.
- CI: an `integration` job against Redis, PostgreSQL, NATS (JetStream), RabbitMQ and ElasticMQ, a
  benchmark build job, and cargo-deny over the whole workspace; a `release.yml` workflow with a
  tag check, the gates, a dry-run publish in dependency order and a gated real publish;
  `scripts/publish-order.txt` and `scripts/publish.sh`.
- Workspace dependencies on `xstate-persistence` and `xstate-eda` are declared with
  `default-features = false` (cargo 1.88 rejects a member that disables defaults the workspace
  entry enables); `xsm` enables `sqlite` itself. The crates keep `sqlite` as their own default.
- The CLI package is `xstate-statemachine-cli` because the crate name `xsm` is already taken on
  crates.io; the installed binary is still `xsm`.
- `xstate-py`: Python bindings (PyO3) backed by the Rust engine. Work in progress; not published
  to crates.io (`publish = false`); CI builds it with maturin and runs its pytest suite.

### Known limitations

- MSRV is 1.88. The `postgres` feature (and `full`, which includes it) needs Rust 1.94 (`sqlx`
  0.9); the `MSRV` CI job checks the workspace and every other feature on 1.88 and skips
  `postgres`.
- The outbox is at-least-once and has memory and SQLite stores only (no PostgreSQL outbox).

### Not yet supported

- Kafka broker adapter.
- `xsm` commands `coverage`, `asyncapi`, `dlq`, `generate-template`, `list-templates`, `new`,
  `plugins`, `update` and `setup` of the Python CLI.
- OpenTelemetry and structlog / Sentry observers (`xstate-observability` covers the Prometheus
  metric set through the `metrics` facade).

### Fixed

- `serde_json` `float_roundtrip` is enabled workspace-wide so wall-clock deadlines survive a JSON
  round trip bit for bit.

### Security

- Dependency tree cleaned for `cargo deny` (advisories, licenses, bans, sources): the AWS SDK now
  uses its `default-https-client` (hyper 1 and rustls 0.23) instead of the legacy `rustls`
  feature, which removes `h2` 0.3, `rustls` 0.21 and `rustls-webpki` 0.101 and their advisories;
  `async-nats` is updated to 0.50 (`rustls-webpki` 0.103). One informational advisory
  (RUSTSEC-2025-0134, `rustls-pemfile` unmaintained, reached through lapin 3) is ignored with the
  reason recorded in `deny.toml`.

[Unreleased]: https://github.com/basiltt/xstate-statemachine-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/basiltt/xstate-statemachine-rs/releases/tag/v0.1.0
