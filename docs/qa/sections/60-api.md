# §6.0 Public API review

Severity tally: CRITICAL 0 · HIGH 0 · MEDIUM 5 · LOW 4. Read-only review: no Rust source,
tests or Python repo changed; only this section file and scratch under `docs/qa/work/api/`
were written.

## Method and evidence boundary

The public surface of every crate was inventoried from `src/lib.rs` re-exports outward, using
`git grep`/`grep` on `pub` items (no `cargo public-api`). For each crate the top-level public
items (structs, enums, traits, type aliases, consts, functions, modules, `pub use`) were
dumped to `docs/qa/work/api/<crate>.txt` (scratch). Key types were then read in full —
`engine/mod.rs` (`Status`, `ActiveState`, `Effect`, `ActionList`, `StepReport`, `start`/`step`),
`interpreter.rs` (`Receipt`, `SyncInterpreter`, `ClockHandle`, `RestoreOptions`), `clock.rs`,
`error.rs`, `machine/mod.rs` (compiled model), `store.rs`, `eda/error.rs` — and cross-crate
usage of the compiled model was traced with grep. `cargo test -p xstate-core --test alignment`
(11 pass), `--test runtime_alignment` (6 pass) and `cargo test -p xstate-statemachine --test
guide` (3 pass) were run against the existing shared `target/` to verify the custom-clock path
(see "Verified sound"); nothing else was built.

The facade `xstate-statemachine` re-exports `xstate_core::*` at its root (so `Machine`,
`Error`, `Receipt`, `Snapshot`, `SimulatedClock`, … all resolve there), the whole of
`xstate_core` under `core::`, and each feature-gated crate under a module of the same name. The
crate-by-crate table below lists the *crates*, not the facade re-exports.

## Per-crate table

| Crate | Public item groups | Verdict |
|---|---|---|
| `xstate-core` | `machine` (compiled model: `Machine`, `StateNode`, `NodeKind`, `Transition`, `Trigger`, `ActionDef`, `Guard`, `InvokeDef`, `AfterDef`, `Builtin`, `Delay`, `SendTarget`); `config` (`MachineConfig`, `StateConfig`, `TransitionConfig`, `GuardConfig`, `ActionConfig`, `InvokeConfig`, `Policies`, `ParseOptions`, 5 policy enums, `ConfigWarning`); `builder` (`MachineBuilder`, `StateBuilder`, `Action`/`Branch`/`Guard`/`History`/`Invoke`/`Transition`/`DelayKey`, `IntoTransitions`); `engine` (`Status`, `ActiveState`, `Effect`, `ActionList`, `StepReport`, `start`, `step`); `interpreter` (`SyncInterpreter`, `Receipt`, `RestartTimers`, `RestoreOptions`, `MAX_DEFERRED`); `logic` (`MachineLogic`, `ActionScope`, fn aliases, `DelayImpl`, `ServiceImpl`); `event` (`Event`, `EventType`, `EventOrigin`, `names`); `context`/`typed` (`Context`, `DynContext`, `TypedEvent`, `TypedState`, `encode`/`object`/`decode`); `snapshot` (`Snapshot`, `Deadline`, `ScheduledSend`, `machine_hash`, name aliases); `graph`/`pure`/`export`/`pyjson`/`redact`/`observer`/`clock`; errors (`Error`, `ErrorKind`, `Result`); consts. | Sound, but see M-API-1/2 (public type aliases + hidden `smallvec`/`smol_str` deps in the compiled model) and L-API-1. |
| `xstate-tokio` | `Interpreter`, `spawn`, `spawn_with_observers`, `Options`, `Overflow`, `View`, `System`. | Sound; `Overflow`/`Options`/`View` not `#[non_exhaustive]` (M-API-4). |
| `xstate-persistence` | `StateStore` trait (+ `LockGuard`, `SaveOptions`, `StoredSnapshot`, `ForgetReport`, `Health`); `MemoryStore`/`FileStore`/`SqliteStore`/`RedisStore`/`PostgresStore` + per-store `*Options`; `locking` (`persisted`, `persisted_retry`, `PersistedOptions`, `Session`, lock strategies, `Jitter`/`RetryPolicy`); `idempotency` (inbox stores + `IdempotencyGuard`); `log` (stores + `TransitionRecord`/`Disposition`/`Origin`); `timers` (`DueTimerScanner`, `ScanResult`, `ScanSummary`); `migration` (`SnapshotMigrator`, `MigrationError`); `receipts` (`ReceiptJson`, `ReceiptError`, `ReceiptErrorKind`, `ReceiptFlags`); `adopt` (`from_state_ids`); `StoreError`/`Result`. | Sound, but `RedisStoreOptions` and `PersistedOptions` skip `#[non_exhaustive]` (M-API-4) and `StoreError::Other(String)` (M-API-3). |
| `xstate-eda` | `envelope` (`Envelope`, `EnvelopeBuilder`, `TransitionOrigin`, `SPECVERSION`, id/validation helpers); `broker` (`BrokerAdapter`, `SyncBrokerAdapter`, `Delivery`, `Subscription`); `dispatcher` (`InboundDispatcher`, `EnvelopeHandler`, `HandlerError`, `OnUnknown`, `DispatchResult`, `Dispatch`); `outbox` (stores + `OutboxPlugin`/`OutboxRelay`/`OutboxRecord`/`PublishSpec`/`PublishSource`); `dead_letter` (stores + `DeadLetterPlugin`/`DeadLetter`/`ErrorEntry`/`ReplayOptions`/`ReplayResult`/sinks); `fake` (brokers + `FakeCore`); `asyncapi` (`asyncapi_document`, `AsyncApiOptions`, validation); broker adapters `redis_streams`/`nats`/`rabbitmq`/`sqs` (+ `*Options`); errors (`EnvelopeError`, `BrokerError`, `EdaError`, `Result`). | Sound, but three error types with `Other(String)` (M-API-3) and all broker `*Options` skip `#[non_exhaustive]` (M-API-4). |
| `xstate-patterns` | `RetryPolicy`/`JitterMode`/`RetryError`; `CircuitBreaker`/`CircuitBreakerBuilder`/`CircuitState`/`Permit`/`wrap`/`CircuitBreakerError`/`CallError`/`AdmitError`/`CircuitOpenError`; `SagaBuilder`/`SagaStep`/`SagaError`; `ChoreographyRouter`/`PersistedHandler`/`Route`/`ChoreographyError`. | Sound; error enums consistently `#[non_exhaustive]`. |
| `xstate-observability` | `MetricsObserver`, `LabelGuard`, `describe_metrics`, `event_label`, label consts, `metric_names` module. | Sound. |
| `xstate-inspect` | `InspectorObserver`, `Sink`/`MemorySink`/`JsonLinesSink`, `SseSink`/`SseOptions`/`SseError`, `protocol`, `replay` (`replay`/`replay_file`/`ReplayOptions`/`ReplayError`), `read_jsonl`/`encode_line`, re-exported `redact`. | Sound; errors `#[non_exhaustive]`. |
| `xstate-macros` | `#[derive(TypedEvent)]`, `#[derive(TypedState)]`, `statechart!` (proc-macro crate — 3 `pub fn` proc-macro entry points, nothing else public). | Sound. |
| `xstate-axum` | `StatechartRegistry`/`RegistryBuilder`/`RegisterOptions`; `RouterOptions`/`router`/`EventGate`/`transition_stream`/WebSocket routes; `problem` (`Problem`, `ApiError`, `problem`/`status_for_error`/`problem_for_error`); `auth` (`Authorizer`, `PrincipalResolver`, `AllowAll`, `Actor`, `Forbidden`); `fanout` (`Subscribers`, `Subscription`, `FanoutItem`); `inspector`; `receipt` helpers; `RegistryError`. | Sound; `ApiError`/`RegistryError`/`RegisterOptions` `#[non_exhaustive]`. |
| `xstate-agents` | `AgentLogic`/`AgentRetry`/`Summariser`; `runner` (`run_agent`, `RunOptions`, `AgentRun`, `AgentSession`, `AgentResult`); `tools` (`ToolRegistry`, `ToolRegistryBuilder`, `Tool`, `ToolSpec`, `ToolFn`, schema validation); `messages` (`ModelCall`, `ModelRequest`, `ModelResponse`, `FakeModel`, `ToolCall`, `Usage`); `multi` (`spawn_agent`, `SpawnAgentConfig`, `SpawnAgent`, `BudgetPlugin`, `handoff_guard`); `structured`; `trace` (`AgentTrace`, `TraceSink`, `TraceIdentity`); `scrub`/`allowlist`/`budgets`/`charts`; `AgentError`/`Result`. | Sound; `AgentError` `#[non_exhaustive]`. |
| `xstate-testing` | `#[doc(hidden)] pub use xstate_core::SNAPSHOT_VERSION` (and nothing else). | M-API-5: publishable but empty. |
| `xstate-conformance` | `Case`/`Step`/`Logic`/`Observation`/`Expected`/`Oracle`/`Divergence`/`ServiceStub`, `LoadError`, `load_case`/`load_cases`/`cases_dir`, `runner`. | Dev-only (`publish = false`); out of semver scope. |
| `xstate-py` | `publish = false`; Python surface `xstate_rs` = `PyMachine`, `PyMachineLogic`, `PySyncInterpreter`, `PyReceipt`, `PySimulatedClock` + `structure_hash()` + `SNAPSHOT_VERSION`/`__version__`. No Rust `pub` items. | Rust semver surface empty; Python surface enumerated at module level only. |
| `xsm` (pkg `xstate-statemachine-cli`) | Binary; no library target. Its public surface is the CLI command/flag set (out of scope here). | N/A. |

## Findings

### M-API-1 (MEDIUM): `ActionList` is a public type alias that freezes `smallvec` and its inline size into the semver surface

- **Where:** `crates/xstate-core/src/engine/mod.rs:299`
  (`pub type ActionList = smallvec::SmallVec<[std::sync::Arc<str>; 2]>`), used at
  `engine/mod.rs:309` (`StepReport::actions`) and `interpreter.rs:38` (`Receipt::actions`).
- **What:** the alias is documented ("Derefs to `[Arc<str>]`", "inline … for up to two actions")
  and ergonomic — `Deref` to `[Arc<str>]` gives `.iter()`/`.len()`/slice indexing, `Clone` and
  `Default` come from `SmallVec`, and `Receipt::action_names()` (`interpreter.rs:93`) bridges to
  `Vec<String>`. That part is fine. The risk is the type itself: it is a public *type alias* to a
  third-party type with the inline capacity `2` baked in. `smallvec` is a workspace dependency
  but is neither re-exported nor declared as a public dependency, so a user who names
  `ActionList`/`StepReport::actions` in their own signature must pull in `smallvec` themselves and
  must match its exact `[Arc<str>; 2]` instantiation. The handover's own "remaining lever 1"
  explicitly plans to "stop reference-counting action names in `ActionList` (API)" — every one of
  those options is a breaking change today because the alias leaks the representation.
- **Recommendation:** wrap it in a newtype
  `pub struct ActionList(smallvec::SmallVec<[Arc<str>; 2]>);` implementing `Deref<Target=[Arc<str>]>`
  (+ `DerefMut`), `Default`, `Clone`, `Debug`, `PartialEq`/`Eq`, `FromIterator<Arc<str>>`,
  `FromIterator<String>`, `From<&[&str]>`/`From<Vec<String>>`, `IntoIterator`, `AsRef<[Arc<str>]>`
  and `as_slice()`. That hides `smallvec` behind a semver-stable name and lets the inline
  capacity change freely. Lighter alternative if the newtype is judged too heavy before 0.1.0:
  `pub use smallvec;` (declared as a public dependency) plus a doc note that the `2` capacity is
  semver-stable. This is the single highest-impact API item and is exactly the public-type change
  §6.0 asks to be judged: it is *documented and ergonomic*, but its *semver posture is not safe*.

### M-API-2 (MEDIUM): the compiled machine model leaks `smol_str`/`smallvec` and hash-format internals through public fields

- **Where:** `crates/xstate-core/src/machine/mod.rs:106` (`StateNode::key: smol_str::SmolStr`),
  `:160` (`Trigger::Event(smol_str::SmolStr)`), `:174` (`Transition::raw_targets:
  smallvec::SmallVec<[smol_str::SmolStr; 1]>`, doc "kept for the structure hash"), `:255`
  (`AfterDef::raw_key`, doc "as written in the configuration"), `:138` (`initial_key`).
- **What:** `smol_str` and `smallvec` are not re-exported by `xstate-core` and are not declared
  public dependencies, yet they appear in the types of public fields, so they are *hidden* public
  dependencies (a downstream crate cannot name `StateNode::key` or `Transition::raw_targets`
  without adding `smol_str`/`smallvec`). Worse, `raw_targets`/`raw_key`/`initial_key` are
  explicitly documented as hash-format internals but are `pub`. `raw_targets` is genuinely read
  cross-crate — `crates/xstate-eda/src/outbox/specs.rs:169` matches `ct.raw_targets.first()` —
  so the CLI/adapters do consume this surface; it is not accidental leakage, but the *shape* is.
- **Recommendation:** expose the raw spellings through methods instead of fields
  (e.g. `Transition::raw_targets(&self) -> impl Iterator<Item = &str>` on `Machine`), make the
  fields `pub(crate)`, and stop exposing `SmolStr` in `StateNode::key` (return `&str` via a
  method or change the field to `Arc<str>`/`String`). At minimum, `pub use smol_str;` / `pub use
  smallvec;` (or the `public-deps` attribute) so the two are tracked semver dependencies.

### M-API-3 (MEDIUM): stringly-typed `Other(String)` catch-alls in `BrokerError` and `StoreError`

- **Where:** `crates/xstate-eda/src/error.rs:58-59` (`BrokerError::Other(String)`), mapped to the
  Python class name `ConnectionError` by `type_name()` at `:69`; `crates/xstate-persistence/src/store.rs:68-69`
  (`StoreError::Other(String)`).
- **What:** §6.3 of the mandate explicitly calls out "no `Other(String)` where a typed variant
  should exist". `BrokerError::Other` is partly defensible — the four broker adapters surface
  heterogeneous driver errors — but it sits next to a rich typed taxonomy and the `type_name()`
  indirection shows the shape is doing untyped work. `StoreError::Other` is harder to defend:
  `StoreError` already has `Unavailable`, `Io(#[from] std::io::Error)`, `Corrupt`, `Engine(#[from]
  xstate_core::Error)`, `NotFound`, `Conflict`, `InvalidKey`, `SnapshotTooLarge`, `LockTimeout` —
  most `Other` call sites should classify into one of these.
- **Recommendation:** audit every `Other` construction and reclassify; keep `BrokerError::Other`
  only if no typed variant fits a given driver, and document it as the deliberate escape hatch.
  Remove `StoreError::Other` and route call sites to `Unavailable`/`Io`/`Corrupt`.

### M-API-4 (MEDIUM): inconsistent `#[non_exhaustive]` posture across options/config structs

- **Where:** `crates/xstate-persistence/src/redis_backend/store.rs:39` (`RedisStoreOptions` is
  *not* `#[non_exhaustive]`) versus `sqlite_store.rs:80`, `file_store.rs:276`,
  `postgres_backend/store.rs:33` (the other three store options *are*). Also not marked:
  `PersistedOptions` (`locking.rs:312`, generic, field-extended during the 0.12.0 alignment — the
  handover itself notes "struct literal users must update"), `StepReport`
  (`crates/xstate-core/src/engine/mod.rs:303`, all-public fields, the type whose `actions` field
  just changed type), and every EDA broker option (`nats/mod.rs:72`, `rabbitmq/mod.rs:78`,
  `redis_streams/mod.rs:68`, `sqs/mod.rs:71`, `asyncapi/mod.rs:34`), plus `GraphOptions`
  (`graph.rs:106`), `ParseOptions` (`config/parse.rs:31`), `RestoreOptions` (`interpreter/restore.rs:35`),
  `Options`/`View`/`Overflow` in `xstate-tokio`.
- **What:** errors and enums are consistently `#[non_exhaustive]`, but the options structs split
  down the middle — even within `xstate-persistence` (`RedisStoreOptions` is the odd one out).
  `StepReport` is the one §6.0 calls out: it is the return value of the public `engine::step` and
  is structurally identical to the type that just had a public field type changed.
- **Recommendation:** make every `*Options`/config struct `#[non_exhaustive]` with a `Default`
  (or a builder), starting with `RedisStoreOptions`, `PersistedOptions`, the EDA broker options
  and `StepReport`. This is the cheap, uniform fix for the exact semver hazard the `ActionList`
  change exposed.

### M-API-5 (MEDIUM): `xstate-testing` is publishable but has no public API

- **Where:** `crates/xstate-testing/src/lib.rs` — the whole crate is `#![forbid(unsafe_code)]` plus
  `#[doc(hidden)] pub use xstate_core::SNAPSHOT_VERSION;`. It is in the publish set (no
  `publish = false` in `crates/xstate-testing/Cargo.toml`) and is wired to the facade's `testing`
  feature.
- **What:** shipping an empty crate at 0.1.0 creates a crates.io package with no usable surface
  and a facade feature that unlocks nothing. The promised helpers (stub logic, simulated-clock
  fixtures, path walking) already live in `xstate-core::graph` (`stub_logic`,
  `state_coverage_targets`, `transition_coverage_targets`).
- **Recommendation:** either drop `xstate-testing` from the 0.1.0 publish set (and the facade
  `testing` feature, pointing the docs at `core::graph`) or actually populate it with the test
  helpers the doc comment promises. Do not publish it empty.

### L-API-1 (LOW): `#[must_use]` missing on six value-returning free functions

- **Where:** `crates/xstate-core/src/graph.rs:237` (`reachable_states`), `:277` (`shortest_paths`),
  `:298` (`simple_paths`), `crates/xstate-core/src/typed.rs:93` (`encode`), `:102` (`object`),
  `:111` (`decode` — returns `Result<T>`).
- **What:** every other value-returning free function in the crate (`to_mermaid`, `to_plantuml`,
  `dumps`, `machine_hash`, `python_event_name`, `engine_event_name`, `redact`, the coverage
  helpers, `stub_logic`) carries `#[must_use]`; these six do not. `decode` returning an unignored
  `Result` is the classic case.
- **Recommendation:** add `#[must_use]` to all six.

### L-API-2 (LOW): `Clone`/`PartialEq` inconsistency across error types (noted, not to fix blindly)

- **Where:** `crates/xstate-eda/src/error.rs:15` (`EnvelopeError`) and `:47` (`BrokerError`)
  derive `Clone + PartialEq + Eq`; `EdaError` (`:75`) and `StoreError`
  (`crates/xstate-persistence/src/store.rs:16`) derive only `Debug + Error`.
- **What:** the split is *justified* — `EdaError`/`StoreError` wrap `std::io::Error` and
  `xstate_core::Error` (containing a `Box<Error>` and `serde_json::Error`), so they cannot derive
  `Clone`/`PartialEq` cheaply. Recorded so the asymmetry is deliberate, not accidental.
- **Recommendation:** no change required; optionally document on `EdaError`/`StoreError` why they
  are not `Clone` while the leaf envelope/broker errors are.

### L-API-3 (LOW): facade `prelude` omits a few everyday items

- **Where:** `crates/xstate-statemachine/src/lib.rs` `pub mod prelude` (~line 145).
- **What:** the prelude has `Event`, `Machine`, `MachineLogic`, `Snapshot`, `SyncInterpreter`,
  `TypedEvent`/`TypedState` and the feature-gated items, but not `Error`, `ErrorKind`, `Result`,
  `SimulatedClock`, `RealClock`, `Clock`, `Receipt`, `RestoreOptions`, `RestartTimers`,
  `PersistedOptions`. All are reachable at `xstate_statemachine::*` (the root `pub use
  xstate_core::*`) — the facade `guide.rs` test uses `xstate_statemachine::SimulatedClock` — but
  "everyday" users writing tests need `SimulatedClock` and `Error`/`Result` constantly.
- **Recommendation:** add `Error`, `ErrorKind`, `Result`, `SimulatedClock` and `Receipt` to the
  prelude (or at least the clock + error trio).

### L-API-4 (LOW): builder/config/machine modules are not promoted to the facade root

- **Where:** `crates/xstate-statemachine/src/lib.rs` — only `pub use xstate_core::*` (the
  *items*) is re-exported; the `builder`/`machine`/`config`/`graph`/`pure`/`snapshot` modules are
  reachable only as `xstate_statemachine::core::builder::MachineBuilder` etc.
- **What:** the builder API (`MachineBuilder`/`StateBuilder`) is the recommended ergonomic
  constructor but is buried under `core::`. The compiled-model introspection types the CLI uses
  are likewise one level deep.
- **Recommendation:** optionally `pub use xstate_core::{builder, machine, config};` at the facade
  root so `xstate_statemachine::builder::MachineBuilder` works; keep the feature-gated modules as
  they are.

## Verified sound

- **`ActionList` ergonomics.** `StepReport::actions` and `Receipt::actions` are both the
  `SmallVec<[Arc<str>; 2]>` alias; it derefs to `[Arc<str>]`, `.iter()`/`.len()` work, and
  `Receipt::action_names()` (`interpreter.rs:93`) and `Receipt::from_parts` (`:74`, `#[doc(hidden)]`
  cross-interpreter seam) consume it correctly. The doc contract ("inline, no allocation, up to
  two actions") matches the definition. The semver risk is M-API-1, not a correctness bug.
- **`SyncInterpreter` no longer holding `Arc<RealClock>` — behaviour unchanged.** `ClockHandle`
  is a private enum (`interpreter.rs:709-724`) with `Shared(&'static RealClock)` and
  `Custom(Arc<dyn Clock>)`, and `Deref` targets `dyn Clock` (delegating `&**clock` in the custom
  arm). `new()` (`:162`) uses `ClockHandle::Shared(shared_real_clock())` — a process-wide
  `OnceLock<RealClock>` borrowed for `'static`; `with_clock()` (`:168`) still takes
  `Arc<dyn Clock>` → `ClockHandle::Custom`. Timer due times are differences of `now_ms()`, so a
  single shared origin is observationally identical to one clock per interpreter (documented at
  `:698-705`). Exercised and green: `crates/xstate-core/tests/alignment.rs:130-134,193-196`,
  `runtime_alignment.rs:28-32,102-127`, `builder.rs:573-583`,
  `crates/xstate-statemachine/tests/guide.rs:98-106`; ran `alignment` (11/11), `runtime_alignment`
  (6/6) and facade `guide` (3/3) against the shared `target/`.
- **Errors.** `Error`/`ErrorKind` (`error.rs`) are `#[non_exhaustive]`, thiserror-based, expose
  `kind()` as a stable serialisable classification, keep `#[source]`/`#[from]` chains
  (`TransitionFailed` sources `Box<Error>`, `Json` from `serde_json::Error`), and implement
  `Send + Sync`. `EdaError`/`StoreError`/`BrokerError`/`EnvelopeError`/`AgentError`/`ApiError`/
  `RegistryError`/`MigrationError`/`ReceiptErrorKind`/`SinkError`/`ReplayError`/`SseError` and the
  patterns errors are all `#[non_exhaustive]` with transparent `#[from]` chains — no swallowed
  `source()`.
- **Enum non-exhaustiveness.** `Effect`, `Builtin`, `HandlerError` and the whole patterns error
  family are `#[non_exhaustive]`; `Status`/`NodeKind`/`Trigger` are small value enums that are
  closed by design. `Machine`/`Snapshot` are `Debug + Clone` (and `Snapshot` `PartialEq`),
  `ActiveState` is sealed (`pub(crate)` fields) with accessor methods, and `StateId`/`TransitionId`
  are `Copy` newtypes with `ROOT`.
- **Feature gating.** `xstate-core` has only the `fast-path` feature (default on, added by
  `903077c`); persistence/eda broker adapters (`redis`/`postgres`/`redis-streams`/`nats`/
  `rabbitmq`/`sqs`) are behind cargo features; the facade re-exports each crate under a
  same-named feature and `prelude` correctly mirrors the always-on + feature-gated items.
- **Compiled model is genuinely consumed, not dead surface.** `xsm` (`commands/{analysis,docs,
  inspect,model}.rs`), `xstate-agents` (`allowlist.rs`, `multi.rs`), `xstate-axum` (`receipt.rs`)
  and `xstate-eda` (`outbox/specs.rs`) read `states()`/`transitions()`/`NodeKind`/`Trigger`/
  `ActionDef`/`raw_targets`, so the public introspection graph is a real cross-package API (its
  *shape* is the M-API-2 concern).
- **Proc macros.** `xstate-macros` exposes exactly the three proc-macro entry points; crate-path
  resolution (`krate.rs`) keeps generated code facade-only (verified in the prior review).
- **`xstate-testing`/`xstate-conformance`/`xstate-py`/`xsm` boundaries.** conformance and py are
  `publish = false`; testing is publishable-but-empty (M-API-5); `xsm` is a binary with no library
  target, so it contributes no Rust semver surface.

## Limits / not covered here

- `xstate-py`'s Python-visible surface was enumerated from `lib.rs` (5 pyclasses + `structure_hash`
  + 2 module attributes) but not method-by-method; its Rust `pub` surface is empty, so it is out
  of the Rust semver scope.
- This is a type/function-level inventory, not a line-by-line audit of every one of the ~1500
  public items; per-file deep dives covered the engine, interpreter, clock, error, compiled-model,
  store and eda-error files, which is where the semver risk concentrates.
- `cargo semver-checks` baseline, `cargo public-api` and `cargo doc`-generated rustdoc JSON were
  not run (tools not installed / shared `target/` in use); the §6.3 semver checklist item remains
  open.
