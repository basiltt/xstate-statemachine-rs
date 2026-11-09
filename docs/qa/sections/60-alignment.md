# §6.0 Python 0.12.0 alignment audit — item by item

**Severity tally: CRITICAL 0 · HIGH 4 · MEDIUM 3 · LOW 5.** Report-only: no repo source, test or
fixture was modified by this section.

Status: complete. Every bullet of the Python changelog's `Added`, `Changed` and `Fixed` sections
for **0.12.0** (2026-11-05) is accounted for below: **220 items** (17 Added, 52 Changed, 151
Fixed). The section file is the only artifact.

## Method and evidence boundary

**Source list.** `C:\Users\basil\Desktop\Projects\PythonProjects\xstate-statemachine\CHANGELOG.md`
§`[0.12.0]` (read-only; saved copy `docs/qa/work/py_changelog_0.12.0.md`); the claimed port is
`CHANGELOG.md` under `Unreleased`, the code and the conformance suite. The Python release is the
authority; the Rust changelog is organised by crate, not by Python item, so each item was mapped
independently to code and tests.

**Classification.**

- **(a) engine semantics** the Rust engine must reproduce — step algorithm, event queue,
  completion, config validation that changes what a chart does. Verified against XState v5 and
  Python.
- **(b) Python-API-only surface** (class DSL, callback shapes, object model, packaging, docs,
  Python-runtime utilities) with a deliberate Rust equivalent (typed DSL / builder / `Observer`)
  or a documented absence — `docs/guide/migrating-from-python.md` (API mapping, "Not ported
  because they target Python frameworks"), `docs/guide/features.md` and
  `docs/guide/stately-editor.md`.
- **(c) persistence / EDA / CLI / web-adapter / agent behaviour** with a Rust analogue in
  `xstate-persistence`, `xstate-eda`, `xsm`, `xstate-axum`, `xstate-agents`.

Verdicts: **MATCH-C** (pinned by a conformance case whose recorded Python expectation is
identical to XState's), **MATCH-T** (pinned by a Rust unit/integration test), **EQUIVALENT**
(different API, same guarantee, with the equivalence stated), **ABSENT-DOC** (not ported and
documented), **PARTIAL→Finding**, **GAP→Finding**.

**The oracle check was run at HEAD.** `"<venv python>" -I conformance/tools/generate_expected.py
--check` printed `53 cases, 0 divergent, 0 stale`. The fixtures record Python **0.12.1** and XState
**5.33.3**; the 0.12.1 release is behaviourally 0.12.0 plus the FileStore lock-stamp change
(Python repo `[Unreleased]`/0.12.1), so the fixtures pin the 0.12.0 semantics. The Rust side is
`crates/xstate-conformance/tests/engine.rs:33` (engine vs XState), `snapshot.rs:67` (structure
hash), `:94`/`:172`/`:213` (snapshot value and text vs Python).

**Evidence boundary.** `cargo` was not run from this section: the shared `target/` is owned by the
§6.1 builds agent (see `docs/qa/work/SESSION-HANDOVER.md`). The committed tests named below were
green in `docs/qa/work/baseline_test_run.log` (alignment, runtime_alignment, parity_0_12,
`rust_engine_matches_xstate_oracle`, axum idempotency/create-if-missing/origin, SSE). The
conformance check above was re-run and is the strongest cross-language evidence. Docker-backed
cross-language tests were not re-run.

**Rust-side claim density.** The `Unreleased` entry does not enumerate the Python items; it lists
crate-level changes. Items it does claim are marked **CHANGELOG** in the verdict.

## Findings

### H1 — Redaction: the core denylist is missing three Python keys, and the transition log uses the pre-0.12.0 substring rule

**Status: FIXED in `376058f`.** The three keys (`oauth`, `creditcard`, `cardnumber`) are in the
core denylist, and the transition-log observer delegates to `xstate_core::redact::RedactKeys`
instead of keeping a second copy, so the whole-word rule applies there too. Tests pin both sides
(`redact::tests::python_0120_denylist_entries_are_present`,
`log.rs::redact_uses_the_python_0120_word_rule_and_entries`).

- **Where:** (i) `crates/xstate-core/src/redact.rs:21` ("Identical to the Python
  `DEFAULT_REDACT_KEYS`") and the list at `:22-62`, matching at `:98-105`;
  (ii) a **second, independent copy** in `crates/xstate-persistence/src/log/observer.rs:15-47`
  (also claiming "the Python `DEFAULT_REDACT_KEYS`"), normalisation at `:59-66` and matching at
  `:72-86`.
- **What:**
  1. Python's `DEFAULT_REDACT_KEYS` has 38 entries including `oauth`, `creditcard` and
     `cardnumber` (`src/xstate_statemachine/plugins.py:840-883`). Both Rust lists have 35 and
     omit those three. Because they are longer than `SHORT_KEY_MAX = 4` in Python they match as
     substrings; in the core list `card`/`auth` are *short* entries, so the undivided key
     `cardnumber` normalises to the single word `cardnumber` and matches neither `card`
     (whole-word only) nor any long entry. `cardNumber` and `card_number` do redact
     (camel/underscore splitting), which is why the existing core unit test passes.
  2. The transition log's copy does **not** implement the 0.12.0 matching rule at all: it
     lowercases and replaces `-` with `_` but never splits camelCase and applies every entry as a
     plain substring (`redact_with`, `:72-86`). With `pin`/`pan`/`card`/`auth`/`otp`/`seed` in
     the list this reproduces the exact over-redaction Python 0.12.0 fixed (F55): `shipping`,
     `company`, `author` and `spinner` are masked in every transition-log record.
- **Reproduction:** `python -I -c "from xstate_statemachine.plugins import redact; print(redact({'oauth':1,'creditcard':2,'cardnumber':3,'card_number':4,'credit_card':5}))"`
  → every value is `'***'`. Applying the core Rust rule (`normalise` + `split('_')` + short
  whole-word / long substring, `redact.rs:98-105`) leaves `oauth`, `creditcard` and
  `cardnumber` untouched. Applying the log's rule (`observer.rs:72-86`) masks `shipping`
  (contains `pin`), `company` (`pan`), `author` (`auth`) and `spinner` (`pin`), which Python
  leaves alone. Redaction feeds the inspector (`xstate-inspect/src/observer.rs:32,91,242`), the
  dead-letter capture (`xstate-eda/src/dead_letter/{mod,plugin,sink}.rs`) and the transition
  log — so the core miss is a leak of three credential spellings and the log copy additionally
  destroys ordinary audit fields.
- **Proposed fix:** add the three entries to the core list; delete the log's duplicate list and
  matcher and reuse `xstate_core::redact::RedactKeys` (the log's `redact(value, keys)` signature
  can keep taking a custom `&[S]`, but compile it through `RedactKeys::new`); add unit cases for
  `oauth`/`creditcard`/`cardnumber` and for `shipping`/`company`/`author`/`spinner` in both
  crates. No golden depends on the list. Severity: **HIGH** (data exposure in the inspector/DLQ
  path plus a false "identical" claim and over-redacted audit records in the log).

### H2 — Invoke `input`/`output` factories do not exist, and two guides say they do

**Status: DOCS FIXED in `f173503`; feature gap OPEN (owner decision).** The migration guide and the
README parity table now record that `invoke.input` is static JSON and the Python 0.12.0
input/output factories are not ported. Implementing them is a public-API addition (a named
input-factory registry plus the triggering event and context as inputs); it is listed as an owner
decision in `REPORT.md`.

- **Where:** `crates/xstate-core/src/machine/mod.rs:243-244` (`InvokeDef.input: Value`, static),
  `crates/xstate-core/src/engine/algorithm.rs:380` (the literal is cloned into the service
  effect), `crates/xstate-core/src/logic.rs:55-56` (`SyncServiceFn = (context, input)`; async at
  `:81-82`), `crates/xstate-core/src/config/parse/items.rs:232` (`input` is any JSON value).
  The claims: `docs/guide/migrating-from-python.md:77` ("a declared `input` factory sees the
  event in both"), `:125-127` ("Property-level `assign` functions, `input` / `output` factories,
  computed delays and callable `sendTo` targets exist only as *named, registered logic*") and
  `docs/guide/stately-editor.md:112-113` ("computed `input`, `output` factories … move them into
  named logic").
- **What:** only *computed delays* actually exist as named logic (`MachineLogic::delay_fn`).
  There is no named `input` resolver, no property-level `assign` function, no `output` factory
  and no callable `sendTo` target anywhere in the workspace (`git grep` for
  `input_fn|input_factory|event_payload` finds only log-record fields). A service therefore
  cannot see the event whose transition entered the state — the Python 0.12.0 **Added** item
  "Services see the triggering event". The workaround is an action that copies the payload into
  the context before entry (the service already receives the context); the event itself is never
  delivered.
- **Reproduction:** compare the service signatures (Rust `(ctx, input)`, Python's injected
  `(context, event, …)`) and search the workspace for an input/factory API (none). The
  conformance fixtures cannot express a factory, which is why the suite does not catch this.
- **Proposed fix (choose one, owner call):**
  1. **Implement the analogue:** `InputSpec::{Static(Value), Named(String)}` on `InvokeDef`,
     `MachineLogic::input(name, |ctx, event| Value)`, resolved where `invoke.input` is cloned at
     `engine/algorithm.rs:380` (and in the tokio child-spawn path,
     `crates/xstate-tokio/src/actor.rs:567-575`); one test with a service that echoes the event.
     Then delete the false claims. This is the parity fix.
  2. **Correct the docs:** say services receive `(context, static input)`, that computed
     `input`/`output` and property-level `assign` are **not** expressible, and that the event is
     available only through an action that records it in context.
  Either way the three sentences above are wrong today. Severity: **HIGH** (false parity claim
  plus a missing Python 0.12.0 engine capability).

### H3 — RabbitMQ does not release its un-acked slot when a settle call fails (Python CM-02 not ported)

**Status: FIXED in `83b0b6e`.** `ack` and `drop_message` release the slot whatever the broker
answers, and the async core now restores a delivery to `inflight` when the transport call fails,
so the caller can retry it with the same `Delivery` (previously the retry was a silent no-op).
Tested by `async_core::tests::a_failed_ack_leaves_the_delivery_retriable`; the full EDA suite
passes with the Docker services up.

- **Where:** `crates/xstate-eda/src/rabbitmq/transport.rs:366-370` (`ack`) and `:374-377`
  (`drop_message`). Counter: `:95`, `release()` at `:237-251`, window at `:338`
  (`prefetch.saturating_sub(unacked)`).
- **What:** `native.ack(...).await.map_err(failed)?;` returns before `self.release()`, so a
  failed `basic.ack`/`basic.reject` leaves `unacked` incremented and the delivery removed from
  the local book. Python 0.12.0 fixed exactly this ("its un-acked counter never recovered; the
  counter is now released even when the ack raises", review CM-02, `contrib/brokers/rabbitmq.py`).
  Rust resets the counter on reconnect (`transport.rs:191`) which covers the stale-tag-after-
  reconnect case, but a settle failure on a still-live channel leaks a slot; after `prefetch`
  such failures `poll` asks for `0` messages and the adapter stops consuming until the channel
  dies. **The delivery is also un-retriable locally:** `Core::claim`
  (`crates/xstate-eda/src/async_core.rs:290-295`) removes the delivery from `inflight` before
  the transport call, so a second `ack`/`nack(requeue=false)` for it returns `Ok(())` as an
  already-settled no-op — the Python 0.12.0 F146 promise ("it can now be retried") is not kept.
- **Reproduction:** `crates/xstate-eda/tests/rabbitmq.rs:484` (`prefetch_bounds_the_unacked_messages`)
  shows the window; there is no test for a failing settle. A test can use the `Transport` seam
  with an `ack` returning `Err` and assert `in_flight()`/a subsequent fetch still progresses.
- **Proposed fix:** `let settled = native.ack(...).await.map_err(failed); self.release();
  settled` (same for `drop_message`), and in `Core::ack`/`drop_delivery` reinsert the
  `Inflight` on a transport error so a retry settles the same handle; add the regression tests.
  Severity: **HIGH** (a broker outage plus an unrecoverable settle after repeated failures).

### H4 — The migration guide says the XState convergence is unreleased and that released 0.12.0 keeps the old engine

**Status: FIXED in `f173503`.** The guide now states that Python 0.12.0 shipped the convergence,
and the port target is stated as 0.12.1.

- **Where:** `docs/guide/migrating-from-python.md:52-70`, especially `:54` ("on its
  `xstate-v5-engine-parity` branch (unreleased after 0.12.0)"), `:69` ("replays the same 50
  cases") and `:70` ("Against released Python 0.12.0 the old table still applies"); the table
  header at `:74` also says "Python (parity branch)".
- **What:** the branch was merged and **released as 0.12.0 on 2026-11-05**; the changelog's own
  `Changed` section lists all eighteen engine changes, and the repo's fixtures record Python
  0.12.1 matching XState on all 53 cases. A migrator reading the guide is told the opposite and
  may keep hand-written workarounds for divergences that no longer exist.
- **Reproduction:** `"<venv python>" -I conformance/tools/generate_expected.py --check` →
  `53 cases, 0 divergent, 0 stale`; compare with the Python `CHANGELOG.md` §0.12.0 `Changed`.
- **Proposed fix:** rewrite the paragraph: the parity branch was merged into 0.12.0; the
  remaining differences are the three rows at `:74-78` (in-memory event spellings,
  `invoke`-without-`input`, multi-target in diagrams/graph); `50` → `53`; drop the "old table
  still applies" sentence. Severity: **HIGH** (false claim about the version the port targets).

### M1 — Same-origin check is case-sensitive where Python 0.12.0 lower-cases both sides

**Status: FIXED in `3ebc6b4`.** The comparison uses `eq_ignore_ascii_case`; allow-listed origins
still match exactly, a non-UTF-8 `Origin` is still refused, and unit tests cover both sides.

- **Where:** `crates/xstate-axum/src/registry.rs:514-527` (`origin_allowed`: `bare.trim_end_matches('/') == host`);
  Python `contrib/_compat.py:103-107` (`bare.lower() == host.lower()`, the single shared rule).
- **What:** `Origin: http://Example.com` with `Host: example.com` is refused (403) by axum and
  allowed by every Python adapter. Fail-closed, so not a vulnerability, but it is the exact
  regression Python 0.12.0 fixed (review CW-33) and it breaks legitimate browser clients behind
  a mixed-case host. The Rust side already has one shared copy, which is the other half of that
  fix.
- **Reproduction:** unit-level: `registry.origin_allowed` with `Origin: http://EXAMPLE.com` and
  `Host: example.com`; `crates/xstate-axum/tests/http.rs:229` only exercises different hosts and
  an exact-case same-origin.
- **Proposed fix:** `bare.eq_ignore_ascii_case(host)` (keep `allowed_origins` exact), add the
  mixed-case case to `state_changing_posts_refuse_cross_origin_and_require_json` and
  `websocket_refuses_cross_origin_with_1008`. Severity: **MEDIUM**.

### M2 — A panic in user logic is not contained: no `on_error`, no teardown, leaked child actors, hung invokes

- **Where:** `crates/xstate-tokio/src/actor.rs:556-566` (an async service future is `tokio::spawn`ed
  and only `ServiceDone` on normal completion; no `JoinHandle` failure is observed — the handles
  in `services: HashMap<String, JoinHandle<()>>` at `:99` are only aborted in
  `stop_all_children` at `:408-412`); the actor run loop itself (`:175-212`) has no
  `catch_unwind`.
- **What:** a panic in a sync action kills the actor task (waiters get `InvalidLifecycle`, but
  the machine never enters `Status::Error`, `Observer::on_error` never fires, and
  `stop_all_children` never runs, so spawned child actors and timers are leaked). A panic in an
  async service is worse: the service task dies, `ServiceDone` is never sent, and the invoking
  state waits forever while the actor stays healthy. Python 0.12.0 hardened the analogous case
  (a `BaseException` escaping the run loop now enters `status == "error"`, fails pending
  receipts, fires `on_error` and tears down).
- **Reproduction:** add an `async_service` that panics and assert the state leaves the invoking
  state; today it does not (no test covers panicking services — HANDOVER §6.2 lists it as a
  target).
- **Proposed fix:** wrap the service future in `AssertUnwindSafe(...).catch_unwind()` (or use
  `JoinHandle` and map `JoinError`), send `ServiceDone { result: Err(Error::logic("panic")) }`;
  for the run loop, either document that panics kill the actor or wrap `core.process` and run
  `shutdown()` + `on_error`. Severity: **MEDIUM** (needs a panicking user callback, but the
  consequences — hung invoke, leaked children — are worse than Python's pre-fix state).

### M3 — Some stranded events are still cleared without `on_event_dropped`

- **Where:** `crates/xstate-core/src/interpreter.rs:335` (`self.deferred.clear()` in `stop()` —
  the deferred lane is dropped silently while `self.queue` at `:331-334` is reported),
  `:534` (`self.queue.clear()` when `onUnhandled: "error"` fires), and the internal-queue clears
  in `crates/xstate-core/src/engine/algorithm.rs:58,92,428`.
- **What:** Python 0.12.0 F99 ("events in the internal queue at `stop()`, and inbox or
  priority-lane events left behind when the async machine completed or failed … Every stranded
  event is now reported, on both engines"). Rust reports the external queue and tokio's mailbox
  (`actor.rs:228-253`), but not the deferred lane at stop and not the queue discarded by the
  error policy. The Rust CHANGELOG claims stop "reports every queued event", which is true only
  of `queue`. The internal-queue case is mostly unreachable in a sans-I/O engine (`stop()` is
  not callable from inside an action), but the deferred/error-policy clears are reachable.
- **Reproduction:** `onUnhandled: "defer"`, queue two events, `stop()` → only the external queue
  is reported; a machine with `onUnhandled: "error"` and a re-entrant send queued → the queue
  vanishes without a report.
- **Proposed fix:** in `stop()`, drain `self.deferred` through `on_event_dropped(.., "stopped")`
  before clearing; in the `UnhandledPolicy::Error` arm, report each queued event as `"stopped"`.
  Add assertions to `runtime_alignment::events_stranded_at_stop_are_reported`. Severity:
  **MEDIUM** (observability contract; no state corruption).

### L1 — `Observer::on_event_dropped` reason tags are undocumented and differ from Python's seven

- **Where:** `crates/xstate-core/src/observer.rs:28-34` ("a short machine-readable tag"); the
  literals in use are `deferred`, `deferred_overflow`, `not_running`, `stopped`,
  `unresolved_target` (`interpreter.rs:332,452,521,525,656`; `tokio/actor.rs:263`). Python
  documents seven (`queue_full`, `not_running`, `stopped`, `chain_budget`, `unresolved_target`,
  `invalid`, `stale_invocation`).
- **What:** the public API contract is a bare `&'static str`; a consumer cannot enumerate the
  values, and `deferred` is reported as a "drop" although the event is parked and later replayed.
  `maxIterations` exhaustion (`RunawayChain`) reports nothing. Rust intentionally has no
  `queue_full`/`invalid`/`stale_invocation` (no bounded sync mailbox, typed events, no Celery).
- **Proposed fix:** enumerate the tags in the trait rustdoc (and distinguish "deferred" from
  "dropped"), and note the Python spellings that have no Rust analogue. Severity: **LOW**.

### L2 — `MachineLogic::validate` reports only the first missing implementation

- **Where:** `crates/xstate-core/src/logic.rs:315-319` ("The first `Error::ImplementationMissing`
  found, in document order"); called from `interpreter.rs:223`.
- **What:** Python 0.12.0 A7 raises one `ImplementationMissingError` listing **every** missing
  guard/action/service. Rust reports one name per start, so a chart with several gaps needs
  several runs. `Error::ImplementationMissing { kind, name }` is single-name by design.
- **Proposed fix:** defer (API change) but record in `migrating-from-python.md` under
  "Configuration checks and runtime rules"; or add a `MachineLogic::missing_names(&Machine) ->
  Vec<(kind, name)>` helper (non-breaking) and use it in the rustdoc example. Severity: **LOW**.

### L3 — A body `idempotency_key` is accepted as event payload instead of refused with 422

- **Where:** `crates/xstate-axum/src/body.rs:20` (`RESERVED_SEND_KEYS = ["wait", "priority"]`),
  `:49-51`; `crates/xstate-axum/src/registry/cycle.rs:226-232` injects the validated header key
  into the payload. Python 0.12.0 F112 refuses a client-supplied `idempotency_key` in the JSON
  body with 422.
- **What:** the Python vulnerability (a body key bypassing header validation) cannot occur in
  Rust because the key is read only from the header and the guard is built from it; a body key
  is just an inert payload field, overwritten when a header is present. The 422 parity behaviour
  is missing, so a migrating client gets 200 instead of a validation error.
- **Proposed fix:** either add `"idempotency_key"` to `RESERVED_SEND_KEYS` (and a test in
  `problem_shapes_for_reserved_keys_size_and_media_type`) or document the deliberate leniency.
  Severity: **LOW** (no bypass).

### L4 — `adopt::from_state_ids` skips named `after` delays when timers are requested

- **Where:** `crates/xstate-persistence/src/adopt.rs:20-24` ("Not ported: Named delays … are
  skipped when `timers` is requested … Only fixed numeric delays get a deadline").
- **What:** Python's `from_state`/`from_snapshot` path can resolve named delays through the
  logic registry; the Rust adoption primitive cannot because it deliberately uses an empty
  registry (`adopt.rs:27-30`). The result is a restorable snapshot with no deadline for a named
  timer — the same shape as a machine whose timer has not been armed, so restore succeeds but the
  timer never fires. It is documented in the module, not in the migration guide.
- **Proposed fix:** document it in the `adopt` row of `migrating-from-python.md`, or add a
  `from_state_ids_with_logic(machine, logic, …)` variant. Severity: **LOW**.

### L5 — Four Python runtime utilities are absent without a word in the migration guide

- **Where / what:** `has_dormant_timers` (F19), `pump_filtered` (C50), `SQLiteStore.close()`
  (F80) and `register_global` (C41). Each is a Python process/object-model concern with a
  structural Rust answer — `RestartTimers::Parked` makes dormancy an explicit restore choice
  (`interpreter/restore.rs:22,176`), timers are per-interpreter/actor so no shared pump exists,
  the SQLite store closes its connections on `Drop` (no manual `close` that can leak another
  thread's connection), and `Observer` registration replaces the global plugin registry. None is
  listed in the guide's API mapping or "Model and API" section.
- **Proposed fix:** one short table in `migrating-from-python.md` ("Python utility → Rust
  equivalent / why there is nothing to port"). Severity: **LOW** (documentation completeness).

## Item-by-item audit

Legend: `core` = `crates/xstate-core`, `tokio` = `crates/xstate-tokio`, `pers` =
`crates/xstate-persistence`, `eda` = `crates/xstate-eda`, `axum` = `crates/xstate-axum`,
`inspect` = `crates/xstate-inspect`, `conf` = `conformance/cases`, `guide` =
`docs/guide/migrating-from-python.md`. `MATCH-C` names the conformance case(s) whose Python
expectation equals XState's; `MATCH-T` names the test.

### Added (17 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| A1 | Pythonic DSL second generation: `sm = Light()` runs, `send`/state/context/`can`/`matches`/`allowed_events`, `with` lifecycle | (b) | `macros` `statechart!` + `TypedEvent`/`TypedState`; `guide:25` maps class DSL → macros/builder. No instance façade (a Rust machine is a value; an interpreter is a separate handle). | ABSENT-DOC (equivalent authoring model) |
| A2 | Events named by the transition attribute (`cycle = green.to(yellow)` → `CYCLE`) | (b) | `statechart!` requires the event name in the transition; `crates/xstate-macros/src/chart/parse/*`; guide API mapping. | ABSENT-DOC |
| A3 | Naming-convention hooks (`on_enter_<state>`, `on_<event>`, `before_/after_<event>`, generic hooks) | (b) | No method-name conventions; explicit `MachineLogic::dynamic().action/guard` and `Observer` hooks (`core/src/observer.rs:13-34`). | ABSENT-DOC |
| A4 | Signature-based argument injection for callbacks, legacy positional shapes kept | (b) | Fixed signatures: guards `(ctx,event,params)`, actions `ActionScope`, services `(ctx,input)` (`core/src/logic.rs:50-56`); guide:15-16. | ABSENT-DOC (typed closures) |
| A5 | `State.Compound` / `State.Parallel` aliases; nested classes accept every `State(...)` keyword | (b) | `statechart!` compound/parallel grammar; the alias problem does not exist in a macro. Builder `core/src/builder/parts.rs`. | ABSENT-DOC |
| A6 | `strict = True` on a `StateMachine`: `UnknownEventError` and `TransitionNotAllowedError` at the call site | (b) | Verified Python-only: a JSON chart with `"strict": true` does **not** raise `TransitionNotAllowedError` (Python snippet, second `send` returned `None`); only the class façade does. Rust `strict` matches the JSON rule (`policies.rs::strict_mode_rejects_undeclared_events_before_processing`, `interpreter.rs:271-283`). | EQUIVALENT |
| A7 | Build-time logic validation listing **every** missing guard/action/service | (b) | `MachineLogic::validate` called from `start()` (`interpreter.rs:223`); `logic.rs:315-349`. Lists the first only. | PARTIAL→**L2** |
| A8 | `MachineNode.to_json(indent=None)`; `TypeError` for non-JSON values | (b) | `Machine::to_json` / `to_json_string` (`core/src/machine/mod.rs:354-361`); the source document is kept and reparsed (fix `290f83a`). A `serde_json::Value` cannot hold a non-JSON value. | EQUIVALENT |
| A9 | Subclassing a `StateMachine` subclass inherits states/transitions/callbacks | (b) | Python class semantics; Rust has no subclassing of machines (compose `MachineBuilder`/macros). | ABSENT-DOC |
| A10 | `engine = "sync" \| "async" \| "auto"`, `clock=`, `plugins=` on the façade | (b) | `SyncInterpreter` / `xstate_tokio::spawn`, `SyncInterpreter::with_clock`, `observe`; guide:17-18,26. | EQUIVALENT |
| A11 | **Services see the triggering event**: `input` factory receives it; `event_payload` opt-in; undeclared input → `{}` (XState `undefined`); child machines too | (a) | `InvokeDef.input: Value` (`core/src/machine/mod.rs:243-244`), cloned at `engine/algorithm.rs:380`; services `(context, input)` (`logic.rs:55-56`); no factory/resolver anywhere. Undeclared input is `null` (documented guide:77). | **GAP→H2** |
| A12 | Plain methods satisfy string references by name (`guard="can_pay"`, `actions=["reserve"]`) | (b) | Explicit registration only; missing names fail at `start()` with `ImplementationMissing` (`logic.rs:287,300`; guide:128-129). No helper scanning. | ABSENT-DOC |
| A13 | `@state.service(on_done=…, on_error=…, id=…, input=…)` decorator | (b) | `statechart!` `invoke` block + `MachineLogic::service`/`machine_service`; builder `Invoke` (`builder/parts.rs:473`). | ABSENT-DOC (same guarantee: one declaration) |
| A14 | `SyncInterpreter.from_state(machine, state, context=…)` / `Interpreter.from_state` — restore without running anything; compound/parallel completes to initial leaves; final → done | (c) | `adopt::from_state_ids` (`pers/src/adopt.rs:185`) + `SyncInterpreter::from_snapshot`; tests `adopt.rs::a_compound_state_without_a_child_enters_its_initial`, `::a_final_state_can_be_adopted_as_done`, `::every_region_of_a_parallel_state_is_entered`, `::illegal_combinations_are_rejected`, `::adoption_agrees_with_what_the_engine_writes_itself`. | MATCH-T (named-delay limitation → **L4**) |
| A15 | Docs: "Coming from python-statemachine"/"Coming from transitions" guides | (b) | Python docs; Rust has `docs/guide/getting-started.md`, `stately-editor.md`. | ABSENT-DOC (Python docs only) |
| A16 | Docs: `jekyll-sitemap` + `robots.txt` | (b) | Python site only; Rust site is §6.4. | ABSENT-DOC |
| A17 | `tests/test_battle_310_pythonic_dsl.py` battle suite | (b) | Python test suite; the Rust equivalents are `core/tests/{alignment,runtime_alignment}.rs`, `tokio/tests/alignment.rs`, conformance. | ABSENT-DOC (Rust has its own) |

### Changed — engine behaviour (18 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| C1 | Exit actions run in reverse document order, one region at a time (descendants before ancestors; last region first) | (a) | `core/src/engine/algorithm.rs:288-321` ("Reverse document order"); `exit_states`; `ActiveState` document order in `engine/mod.rs:116`. | MATCH-C `conf/020_exit_order_reverse_document.json` |
| C2 | Entry set entered in document order | (a) | `algorithm.rs:323-357` `enter_states`; per-machine entry sets `engine/sets.rs:431`. | MATCH-C `conf/021_parallel_entry_order.json` |
| C3 | A transition into a sibling region resets the whole parallel state's regions (domain = parallel; every region exits/re-enters; parallel itself not re-entered) | (a) | `engine/algorithm.rs` microstep + `compute_exit_set`/entry set; `engine/sets.rs`. | MATCH-C `conf/028_parallel_cross_region_target.json` (actions `x_c,x_r2,x_a,x_r1,e_r1,e_a,e_r2,e_d`, no `x_p`/`e_p`) |
| C4 | `reenter: true` re-enters the source even when the target is its child; root re-entry runs root actions | (a) | Transition `reenter` compiled (`config/parse/items.rs:172-177`); `algorithm.rs` domain above source. | MATCH-C `conf/030_reenter_descendant_target.json` |
| C5 | Completing the machine runs remaining exit actions (final state + root, after output); also when `start()` lands on a final state | (a) | `algorithm.rs:33-43` `exit_all_when_done`, `:425` `finish`; `core/tests/start_in_final.rs::start_in_a_final_state_runs_every_exit_action`. | MATCH-C `conf/029_exit_actions_on_completion.json` |
| C6 | `"*"` / `"prefix.*"` also receive engine events (`done.invoke.*`, `error.platform.*`, `after.*`, `done.state.*`), own handlers outrank, deeper wildcard outranks ancestor | (a) | Descriptor matching `engine/select.rs`; engine events are ordinary events on the internal queue (`algorithm.rs`). | MATCH-C `conf/027_wildcard_vs_engine_events.json` |
| C7 | Multi-target transitions into different regions of one parallel state; other list shapes refused; `target_strs`, hash and diagrams use the first | (a) | `config/parse/items.rs` target list; `machine/compile.rs`; `Machine::to_json` keeps the list. | MATCH-C `conf/031_multi_target_parallel_entry.json`; `core/tests/alignment.rs` + `export.rs` |
| C8 | JSON `choose` runs the first matching branch; `stateIn` accepts `params.stateValue`; branch guards composite | (a) | `engine/actions.rs:194-210` `Builtin::Choose`; guard compilation `config`/`machine`. | MATCH-C `conf/032_choose_and_state_in.json` |
| C9 | Transitions for one event form one microstep (grouped exit → actions → entry); conflicting transitions dropped (document order, descendant wins) | (a) | `algorithm.rs:207-252` `microstep`; `removeConflictingTransitions` logic in `select`/`sets`; fast path preserves the report (`fast_path_tests`). | MATCH-C `conf/038_conflict_document_order_preemption.json`, `conf/039_microstep_groups_exit_actions_entry.json`, `conf/022`, `conf/023` |
| C10 | `done.state.*` goes on the internal queue (before queued external events); rollback withdraws it; counts toward `maxIterations` | (a) | `algorithm.rs:386-424` `final_state_entered` pushes to `internal_queue`; `settle` `:71-115`. | MATCH-C `conf/041_done_state_before_queued_external_event.json`, `conf/024_done_state_queue_order.json` |
| C11 | Guards evaluated in XState order and only when needed (per region, deepest first, first enabled; ancestor only if no region matched); `always` per leaf; `can(event)` same as `send` | (a) | `engine/select.rs` (selection walk); `engine/algorithm.rs` eventless pass; oracle records guard evaluations. | MATCH-C `conf/042_guard_evaluation_order.json` |
| C12 | One `assign` computes every property against the context before it; separate assigns run in order | (a) | JSON `assign` values are data and merged by `Context::assign_json` (`engine/actions.rs:124-128`); there are no per-property callables in Rust (ADR-0002), so the rule is unobservable. Separate-assign order pinned. | MATCH-C `conf/043_assign_actions_apply_in_order.json` (the one-assign rule is not expressible in JSON) |
| C13 | A compound state targeting itself without `reenter` resets its children (source is the domain) | (a) | `algorithm.rs` microstep (`computeEntrySet` of the source); `ActiveState`. | MATCH-C `conf/044_compound_self_target_without_reenter.json` |
| C14 | JSON `assign` without the `assignment` wrapper updates the context | (a) | `config`/`machine` built-in compilation reads `params` or `params.assignment`; `actions.rs:124`. | MATCH-C `conf/045_assign_without_assignment_wrapper.json` |
| C15 | `always` transitions checked in the order their states were **entered**; re-entry that changes nothing keeps the old order; restore starts in document order | (a) | `ActiveState::entered_at` (`engine/mod.rs:207`; `algorithm.rs:323-357`); eventless leaves sorted by stamp. | MATCH-C `conf/046_always_order_follows_entry_order.json`, `conf/050_always_order_after_reentry.json`; `core/tests/eventless.rs::leaves_are_visited_in_entry_order_and_a_restore_resets_it` |
| C16 | History recorded only when its owner is exited | (a) | `algorithm.rs:288-305` records history before exit, keyed by exited state. | MATCH-C `conf/047_history_records_only_on_owner_exit.json` |
| C17 | An `always` that changes nothing runs once, not in a loop (macrostep re-selects only after a change) | (a) | `algorithm.rs:71-115` `settle` + `note_snapshot_change` (`:264-285`), `ActiveState::mutations`. | MATCH-C `conf/048_always_self_target.json`; `core/tests/eventless.rs::an_action_that_changes_nothing_runs_once_per_macrostep`, `::a_user_action_changing_the_context_keeps_a_targetless_always_selected` |
| C18 | `done.state.*` reaches the compound parent then every completed parallel ancestor; never a compound grandparent; a history child is not a region | (a) | `algorithm.rs:386-424` `final_state_entered`, `in_final_state` (`:432-450`) skips history; `settle` delivers. | MATCH-C `conf/049_done_state_propagation.json` |

### Changed — Python API, DSL, docs (11 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| C19 | A `StateMachine` instance owns `status/state/context/configuration/machine/interpreter/allowed_events` attributes | (b) | No façade; `Interpreter` methods (`state_ids`, `context`, `view`). Python class semantics only. | ABSENT-DOC |
| C20 | Naming-convention hooks bind existing undecorated methods too | (b) | As A3; explicit registration. | ABSENT-DOC |
| C21 | `internal: false` is the v4 alias of `reenter: true` (child target exits/re-enters the source) | (a) | `config/parse/items.rs:172-177` (`reenter.or(internal.map(\|i\| !i))`); `config/parse/tests.rs:112-121`. | MATCH-T (alias) + MATCH-C `conf/030` for the behaviour |
| C22 | `subscribe()` listeners notified once per microstep (settled configuration, not per region) | (a) | `xstate-tokio`: one `view_tx.send_replace` per processed event (`tokio/src/lib.rs:151`, `actor.rs:218-220`). | MATCH-T `tokio/tests/alignment.rs::subscribers_see_targetless_transitions` (watch coalescing gives one notification per macrostep) |
| C23 | A declared falsy `input` (`0`, `""`, `[]`, `false`) reaches the service as declared; only undeclared/`null` is the empty value | (a) | `InvokeDef.input` is a `Value` cloned verbatim (`machine/mod.rs:243-244`, `algorithm.rs:380`); absent → `null` (documented guide:77). | EQUIVALENT (no dedicated falsy-input test; behaviour is a direct clone) |
| C24 | `xstate_statemachine.pythonic` re-exports only its own names | (b) | Python module mechanics. | ABSENT-DOC |
| C25 | `State.to()`/`State.internal()` infer the event from the attribute; a still-nameless transition fails at build time | (b) | `statechart!` requires an explicit event at compile time. | ABSENT-DOC (compile-time) |
| C26 | Reserved attribute names in a `StateMachine` body (`send`, `state`, `start`/`stop` rules) | (b) | Macro grammar reserves its own keywords; `cargo check` is the gate. | ABSENT-DOC |
| C27 | Lighter `import xstate_statemachine` (PEP 562 lazy imports) | (b) | Python import-time concern only. | ABSENT-DOC |
| C28 | Docs: one recommended Quick Start (`with Machine() as sm:`) | (b) | `docs/guide/getting-started.md` (Rust first machine). | ABSENT-DOC |
| C29 | `pythonic` moved to private modules (`_pythonic_core`, …) | (b) | Python module layout. | ABSENT-DOC |

### Changed — pre-release review pass (23 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| C30 | Exit actions run before the state's invokes/children are stopped, so an exit action can `sendTo` its own invoke | (a) | `tokio/src/actor.rs:405-410` (stop children only after the step's effects) and effect ordering; `tokio/tests/alignment.rs::exit_action_reaches_the_states_own_invoke`. | MATCH-T |
| C31 | `SyncInterpreter` non-blocking child completion/sendParent delivered on the owner thread | (b) | The sync interpreter has no actor system or pump threads (ADR-0004; guide:130-132); machines invoked as services run on tokio, which owns the task. | ABSENT-DOC (architecture removes the hazard) |
| C32 | State `type` outside the five kinds, and `compound`/`parallel` without children, are `InvalidConfigError`; an atomic/final/history with children becomes compound with a warning | (a) | `config/parse.rs` `parse_state`; `config/parse/tests.rs:61-84`. | MATCH-T |
| C33 | Invalid `history` value refused (no silent shallow fallback); history default target non-empty string | (a) | `config/parse.rs`; `config/parse/tests.rs:87-96`. | MATCH-T |
| C34 | `strict`, `strictTargets`, `strictConfig`, `reenter`, `internal` accept only `true`/`false`/`null` | (a) | `config/parse/items.rs:54-70,172-177`; `config/parse/tests.rs:99-122`. | MATCH-T |
| C35 | Config errors name the state and key | (a) | `config/parse.rs` paths; `config/parse/tests.rs:125-134`. | MATCH-T |
| C36 | Action object without `type` refused; non-object `params` refused; unknown action/guard/params keys warn or error under `strictConfig` | (a) | `config/parse.rs`; `config/parse/tests.rs:137-160`; flat built-in params refused with a hint (`:52-58`). | MATCH-T |
| C37 | `maxIterations` whole number ≥ 1 (integer or `5.0`); `spawnBlockingTimeout` finite ≥ 0 | (a) | `config/parse.rs:299-310`; `config/parse/tests.rs:183-198`. Python also accepts `"5"` — documented deliberate difference (guide:105-106). | MATCH-T + documented difference |
| C38 | `initial` naming no child refused at `create_machine` with a "did you mean" hint | (a) | `config/parse.rs`; `config/parse/tests.rs:201-205`. | MATCH-T |
| C39 | `after` keys whole, non-negative ms or a delay name; numeric strings normalised; duplicates/frac/negative refused; the timer event uses the normalised value | (a) | `config/parse.rs`; `core/tests/alignment.rs::after_keys_are_normalised_and_validated`; `AfterDef::raw_key` (`machine/mod.rs:254-263`). | MATCH-T + MATCH-C `conf/053_after_key_normalisation.json` |
| C40 | Invoke `id`/`systemId` non-empty, `description` string, child keys non-empty, empty `target` refused, `entry`/`exit`/`actions` of `0`/`false`/`""` refused | (a) | `config/parse.rs:623-629`, `config/parse/items.rs:163-180,209-213`; `config/parse/tests.rs:163-180`. | MATCH-T |
| C41 | Runtime utilities: `register_global` TypeError; `ExplorationLimitError` exported; `receipt_to_json(include_messages=)` + MRO status; `STATUS_NO_OP`; typed clock advance | (b) | `receipts.rs:27-28,56-59,217-253,340`; `clock.rs:78,97` + `SyncInterpreter::advance`; graph exploration uses `ErrorKind::LimitExceeded` (`graph/search.rs:89`); no global registry (Observer). | EQUIVALENT (register_global absent → **L5**) |
| C42 | Every `PluginBase` hook docstring states engine/thread/permissions; `on_event_dropped` documents all seven reasons | (b) | `core/src/observer.rs:13-34` (one-line hooks, no reason list). | PARTIAL→**L1** |
| C43 | `after_<event>` / `after_transition` hooks run once per event, after the whole macrostep | (b) | No naming-convention hooks; `Observer::on_event` fires once per processed event (`interpreter.rs:511-525`). | EQUIVALENT |
| C44 | `pythonic.__all__` lists only public names | (b) | Python module mechanics. | ABSENT-DOC |
| C45 | Persistence utilities: `fingerprint()` refuses values whose text embeds a memory address; `adopt.from_state_ids` refuses non-JSON context; `list_versions(limit=-1)` ValueError; `persisted` typed | (c) | `fingerprint` takes `&Value` (`pers/src/idempotency.rs:143`) so a non-JSON object cannot be passed; `from_state_ids(context: Value)`; `list_keys(prefix, limit: usize)`; `persisted` closure is typed. | EQUIVALENT (typed API makes the failure modes unrepresentable) |
| C46 | `xsm coverage --json` writes `--fail-under` failures to stderr | (b) | `xsm` has no `coverage` command — documented absent (`guide:156-160`, README matrix). | ABSENT-DOC |
| C47 | Web adapters: operationId stability note | (b) | `xstate-axum` generates no OpenAPI document, so there are no operationIds. | ABSENT-DOC |
| C48 | Django & SQLAlchemy `PermissionGuard` raises instead of silently denying; Django no longer imports `_make_id` | (b) | Python frameworks not ported (`guide:44-46`). | ABSENT-DOC |
| C49 | `last_error`/`receipt.error` for an unresolved `sendTo`/`forwardTo`/`sendParent`/`escalate` is `UnresolvedTargetError` (subclass of `ActorSpawningError`) | (a) | `Error::UnresolvedTarget` / `ErrorKind::UnresolvedTarget` (`core/src/error.rs`); soft error on `Receipt::error`/`StepReport::soft_error`; `receipts.rs:56-59,340` maps it to HTTP 500; `core/tests/alignment.rs::unresolved_targets_are_reported_and_mark_the_step`, `receipts.rs::status_no_op_is_a_200_and_unresolved_target_is_a_500`, `tokio/tests/alignment.rs::unresolved_targets_are_reported_and_mark_the_step`. | MATCH-T |
| C50 | `RealClock`/`SimulatedClock.pump_filtered(owner_filter)`; `Clock.pump()` unchanged | (b) | No shared deadline heap: timers belong to one interpreter/actor (`SyncInterpreter.timers`, tokio `Core.timers`), so there is nothing to filter. | ABSENT-DOC (architecture) → **L5** |
| C51 | `create_machine()` warns for `logic` + `logic_modules`; `event_schemas` validated at build; alias warnings name the caller's line | (b) | No module scanning, no `event_schemas`, no aliases: logic is an explicit registry and `MachineLogic::validate` checks names at `start()`. | ABSENT-DOC |
| C52 | Python-side performance (25 % cheaper build, 20 % cheaper start, etc.) | (b) | Python-only measurements; Rust performance is §6.3 (S1–S7). | ABSENT-DOC (Python concern) |

### Fixed — engine, config, runtime, persistence, EDA (94 items before the CLI block)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| F1 | Nested-class transitions dropped; double compilation; nested class as `.to()` target | (b) | Python class compiler; Rust JSON keys are paths and cannot alias. | ABSENT-DOC (no analogue) |
| F2 | Same-named states under different parents shared transitions (class compiler keyed by bare name) | (b) | Same as F1; Rust ids are paths. | ABSENT-DOC |
| F3 | Mermaid/PlantUML: `after` edges (numeric and named, `[guard]`), `--> [*]` final markers, `--` parallel divider, `invoke onDone`/`onError`, `always` labels | (b) | `core/src/export.rs:19-26,202`; 72 Python-generated goldens under `core/tests/fixtures/export/`; `core/tests/export.rs::mermaid_matches_python_golden_files`, `::plantuml_matches_python_golden_files`. | MATCH-T (golden) |
| F4 | Three-argument legacy guard `(context, event, params)` received two args when no `params` declared | (b) | Rust guards always receive `&Value::Null` when no params (`logic.rs:52`, `engine` invocation). | EQUIVALENT |
| F5 | A strict façade refuses a send to a finished/stopped machine with `InterpreterStoppedError` | (b) | No façade; `process` reports `on_event_dropped("not_running")` and returns an unhandled receipt (`interpreter.rs:445-455`), tokio same (`actor.rs:260-271`). Guide:130-132 (tokio handle is the driver). | EQUIVALENT (observer, not exception) |
| F6 | Docs: Getting Started CLI/tree/Python ranges/search index | (b) | Python docs/site. | ABSENT-DOC |
| F7 | `State(entry="name")`, `exit=`, `.to(actions="name")` treated a string as an iterable of chars | (b) | Rust configuration strings are single action names; `statechart!` is typed. | ABSENT-DOC (impossible) |
| F8 | Pythonic guide `storeError`/`FormWizard` docs | (b) | Python docs. | ABSENT-DOC |
| F9 | A state-level `onDone` list: every alternative is a candidate, failing guards fall through | (a) | `config/parse.rs` parses all alternatives; `machine` keeps the list; export draws every alternative. | MATCH-C `conf/051_on_done_alternatives_fall_through.json`; CHANGELOG |
| F10 | A child sharing the parent's `SimulatedClock` fired the parent's deadlines | (b) | No child pumps in the sync engine (C31). | ABSENT-DOC |
| F11 | Uncopyable initial context made `deepcopy` raise; now shallow copy + warning | (b) | Rust context is moved/`Clone`d by the type system; no `deepcopy` exists. | ABSENT-DOC (impossible) |
| F12 | Auto-discovery: staticmethods, built-in-named user functions, `spawn_<key>` → services | (b) | Explicit `MachineLogic` registration; user action wins over a built-in (`core/tests/alignment.rs::user_actions_win_over_built_ins`); `spawnChild.src` is a service name (`engine/actions.rs:184-192`). No scanning to get wrong. | EQUIVALENT |
| F13 | Pydantic nesting cap names `recursion_depth_exceeded` | (b) | Pydantic not ported; serde_json enforces a 128-level parse limit (`axum/src/body.rs:113-117` maps to 422). | ABSENT-DOC |
| F14 | Django management command repeats the CLI message | (b) | Django management commands not ported. | ABSENT-DOC |
| F15 | Action failures inside `choose`, `pure`, `enqueueActions` follow `actionErrorPolicy` | (a) | `choose` yes: `core/tests/alignment.rs::choose_failures_follow_the_action_error_policy`; `pure`/`enqueueActions` are unsupported and documented (`docs/guide/stately-editor.md:111-113`). | MATCH-T (choose) + ABSENT-DOC (pure/enqueueActions) |
| F16 | Entry action failing on a top-level final under `rollback`/`fail`: completion only on commit | (a) | `core/tests/alignment.rs::failing_entry_on_a_top_level_final_state_does_not_complete`. | MATCH-T |
| F17 | A `raise` inside an initial-state `always` is processed after the start-up microstep | (a) | `engine/algorithm.rs:19-31` start + `settle`. | MATCH-C `conf/052_raise_in_initial_always.json` |
| F18 | `subscribe()` notified for targetless transitions | (a) | One publish per processed event. | MATCH-T `tokio/tests/alignment.rs::subscribers_see_targetless_transitions` |
| F19 | `has_dormant_timers` per state | (b) | No dormant-set API; `RestartTimers::Parked` (`interpreter/restore.rs:22,176`) leaves deadlines in the snapshot and `pending_timers()` (`interpreter.rs:396`) counts armed ones. | ABSENT-DOC → **L5** |
| F20 | Child actors restored with `minimum_version`/`expected_machine_hash`/plugins | (b) | Persistence restores through `SyncInterpreter` (no child actors); the tokio interpreter has no `from_snapshot` (guide:22 maps restore to `SyncInterpreter`), so no child record can be restored with wrong options. | ABSENT-DOC (architecture) |
| F21 | `restart_timers` resume/fire_due refused before arming anything | (a) | `interpreter/restore.rs:158-192` validates each deadline before arming; a bad one returns `Error::Snapshot` and the half-built interpreter is dropped. | MATCH-T `runtime_alignment::deadline_of_a_state_without_after_refuses_the_restore` |
| F22 | `send("")` raises `InvalidEventError` | (a) | `interpreter.rs:263-265`; `Event::from_json` same. | MATCH-T `core/tests/alignment.rs::an_empty_event_type_is_invalid`, `tokio/tests/alignment.rs::empty_event_type_is_invalid` |
| F23 | Keyword payload with an `Event` object, or an `__xstate_event__` adapter returning an Event | (b) | `Event::with_payload` is the explicit API; typed events via `TypedEvent`. | EQUIVALENT |
| F24 | An `assign` callable returning a non-mapping is an action error | (a) | Rust actions return `Result<()>`; JSON `assign` is a map by construction (`actions.rs:124`); non-object context is an action error (below). | EQUIVALENT |
| F25 | `sendParent`/`escalate` without a parent fire `on_event_dropped("unresolved_target")` and fail the step | (a) | `core/tests/alignment.rs::unresolved_targets_are_reported_and_mark_the_step`, `tokio/tests/alignment.rs::...`. | MATCH-T |
| F26 | Inline delay callable raising is contained; a delay of `True`/NaN/inf/non-number is rejected on every path | (a) | `core/tests/alignment.rs::unusable_delays_disable_only_their_timer` (NaN, inf, failing function, on `after` + delayed `raise` + delayed `sendTo`); `machine/builtin.rs:213-219` refuses non-number/non-name delays. JSON cannot encode NaN. | MATCH-T |
| F27 | Snapshot `taken_at` uses the clock's `wall_now()` | (a) | `interpreter.rs` snapshot uses the interpreter clock; tokio uses the system wall clock (documented, guide:115-117). | MATCH-T `runtime_alignment::snapshot_taken_at_uses_the_interpreter_clock` |
| F28 | Sync events queued after the machine reached final/stopped are not processed; stranded events reported | (a) | `interpreter.rs:445-455` `process` checks status; `stop` reports the external queue (`:331-334`). | MATCH-T `runtime_alignment::events_stranded_at_stop_are_reported` |
| F29 | `stop(drain=True)` from inside an action | (b) | Actions receive `ActionScope`, never the interpreter, so a re-entrant stop is unrepresentable. | ABSENT-DOC (impossible) |
| F30 | Events in the internal queue at `stop()`, and inbox/priority-lane events at completion/failure, reported | (a) | Partial: external queue and tokio mailbox reported; `deferred` and the `onUnhandled: error` queue cleared silently (`interpreter.rs:335,534`). | PARTIAL→**M3** |
| F31 | A throwing `send_threadsafe` event did not abort the owner's `send` | (b) | No `send_threadsafe`; the tokio handle is `Send + Sync` and misuse is a compile error (guide:130-132). | ABSENT-DOC (impossible) |
| F32 | Child pump with `RealClock` fired the parent's `after` timers | (b) | Per-actor timer heaps (`tokio/src/actor.rs` `Core.timers`). | ABSENT-DOC (architecture) |
| F33 | `spawn_blocking_*` waited the full timeout | (b) | No blocking spawn API; `spawnBlockingTimeout` is parsed for chart compatibility and inert (`config/parse.rs:299-310`). | ABSENT-DOC |
| F34 | Async `__call__` action objects / functions returning coroutines | (b) | `async_service` is the explicit async form; sync actions cannot return a future (type system). | ABSENT-DOC (typed) |
| F35 | Async `send_events()` ignored `OverflowPolicy.BLOCK` and self-send routing | (c) | No batch API; each `send`/`try_send` applies the policy (`tokio/src/lib.rs:162-196`). | EQUIVALENT |
| F36 | A non-`Exception` `BaseException` escaping the async run loop tore down nothing | (a) | No panic containment around the actor loop or service futures (`actor.rs:170-212,556-563`). | **GAP→M2** |
| F37 | A failed `start()` left the async machine stopped without release and the sync machine running half-entered | (a) | `engine/mod.rs:370-403` `start_into` clears the slot on error; `interpreter.rs:229-238` clears effects; a retry is possible. | MATCH-T (by construction; `persisted` also checks: `locking.rs::refuse_failed_start`) |
| F38 | No `on_service_*` hooks for invoked child machines; an unhandled child-start failure left the parent running | (a) | `Observer` has no service hooks (events `done.invoke.*`/`error.actor.*` are visible through `on_event`); an unhandled `error.actor` fails the machine (`engine/algorithm.rs:45-60`). | EQUIVALENT |
| F39 | `SyncInterpreter.send_events()` ran threadsafe events/timers after the batch instead of before | (b) | No batch API; `send` drains due timers first (`interpreter.rs:284-312`). | ABSENT-DOC |
| F40 | Re-starting a restored async interpreter in done/error fired `on_interpreter_start` every time | (b) | No restart API (a tokio actor is spawned once); `on_start` is emitted once (`actor.rs:171`). | ABSENT-DOC |
| F41 | A delayed `sendTo` to another actor reported failure only as a task exception | (a) | Delayed `sendTo` re-resolves at fire time and reports `unresolved_target` (`tokio/tests/alignment.rs::...` `LATER` case). | MATCH-T |
| F42 | After an `actionErrorPolicy` rollback, history is restored and re-arming follows document order | (a) | `core/tests/alignment.rs::rollback_restores_history`; entry/exit order is document order (`engine/sets.rs`). | MATCH-T |
| F43 | `sendTo("kid")` from an actor named `kid` matched grandchildren; a child keyed `parent` shadowed the real parent | (b) | Deliberate difference documented: Rust resolves only its own invoke/spawn ids, then `systemId`, and `parent` always means the parent (guide:111-114). | ABSENT-DOC (nothing to fix) |
| F44 | Building a machine with an `invoke` configured the host ROOT logger | (b) | Rust logs through `tracing` and never installs a subscriber. | ABSENT-DOC (impossible) |
| F45 | A state-level `onDone` list kept only the first entry | (a) | All alternatives parsed and kept; export draws them. | MATCH-C `conf/051` |
| F46 | A user action named like a built-in with its own `params` was rejected by the built-in's check | (a) | `Machine::from_value_with_logic` (`core/tests/alignment.rs::user_actions_win_over_built_ins`); `MachineLogic::action_names`. | MATCH-T + CHANGELOG |
| F47 | Restoring a snapshot with a pending engine completion emitted a DeprecationWarning | (a) | Engine records `"engine": true` restore as engine events without warnings. | MATCH-T `runtime_alignment::corrupt_event_records_refuse_the_restore` (the accepted records include `done`/`after`) |
| F48 | `get_state_by_id()` failed when the machine id or a state key contained a dot | (a) | Dotted ids resolve; `Machine::state_by_id`. | MATCH-T `core/tests/alignment.rs::dotted_ids_resolve` |
| F49 | A dotted custom id could not be targeted as `#my.id` / `#my.id.child` | (a) | Longest dotted prefix resolution. | MATCH-T `core/tests/alignment.rs::dotted_ids_resolve` |
| F50 | `restore_event` accepted an unknown `kind`, a non-object payload, a bare TypeError | (a) | `runtime_alignment::corrupt_event_records_refuse_the_restore` (unknown kind, list payload, empty type, string record, bad timer timestamp → `ErrorKind::Snapshot`); valid `system`/`done` records restore. | MATCH-T + CHANGELOG |
| F51 | `Clock.pump()`/`SimulatedClock.increment()` dropped timers after one raised and fired a cleared timer; NaN/inf refused | (a) | `runtime_alignment::simulated_clock_batches_and_rejects_non_finite_times` (a timer cancelled in the same batch does not fire; `try_increment`/`try_set`/`advance` refuse non-finite); `clock.rs:78,97`. | MATCH-T |
| F52 | The pure transition API's probe cache kept machines alive; `transition()` revived a done/error snapshot | (a) | The pure API is stateless functions; `runtime_alignment::pure_transition_leaves_a_finished_state_alone`. | MATCH-T |
| F53 | `wait_for_sync` spun forever on a stopped machine; waiters raise `WaitTerminatedError`; `to_promise` | (b) | No `wait_for`/`to_promise` in Rust; `tokio::Interpreter::wait_done` returns immediately for a non-running status (`lib.rs:221-230`). | ABSENT-DOC |
| F54 | Library exceptions could not be pickled | (b) | Rust error types are not pickled (no multiprocessing/Celery). | ABSENT-DOC |
| F55 | `redact()`/`LoggingInspector` crashed on namedtuples/cycles and over-redacted `shipping`/`company`/`author`/`spinner`; short entries match whole words | (b) | Core `redact.rs:98-157` implements the rule and its unit tests; a `Value` cannot be a namedtuple or self-referencing. **But the core default list is incomplete and the transition-log copy (`pers/src/log/observer.rs:72-86`) still applies the old substring rule.** | PARTIAL→**H1** |
| F56 | `TaskManager.cancel_by_owner` orphaned/leaked entries | (b) | No `TaskManager`; tokio tasks are owned by the actor (`Core.services`) and aborted in `stop_all_children`. | ABSENT-DOC |
| F57 | `from_iterator` false warning; `send_back` losses | (b) | No iterator/`send_back` adapter; `sendTo` reports unresolved targets. | ABSENT-DOC |
| F58 | `logic_names(config)` vs `logic_names(machine)` disagreed; `spawn_<key>` reported as an action | (b) | One discovery path (`graph::stub_logic`, `graph/stub.rs:111-132`) over a compiled `Machine`; `spawnChild.src` is a service. | ABSENT-DOC |
| F59 | A plain `async def` guard bound by name was truthy; now refused at build | (b) | Guards are `Fn(&C,&Event,&Value)->Result<bool>` (`logic.rs:52`); an async guard does not type-check. | ABSENT-DOC (typed) |
| F60 | `engine="auto"` selected the sync engine for a plain async action | (b) | The driver is chosen by the caller (`SyncInterpreter` vs `spawn`), not inferred. | ABSENT-DOC |
| F61 | A decorated callback named like a façade member replaced it | (b) | No façade; registry names are independent of interpreter methods. | ABSENT-DOC |
| F62 | A state named `start`/`stop` broke `sm.start()`/`sm.stop()` | (b) | No instance façade; state ids are data. | ABSENT-DOC |
| F63 | Inferred event names kept SCREAMING_SNAKE/acronyms | (b) | No name inference; `statechart!` names events explicitly. | ABSENT-DOC |
| F64 | `internal=True` with a target raised in the Pythonic builder; `transition(target=None)` | (b) | No Pythonic builder; the JSON alias is parsed (C21). | ABSENT-DOC |
| F65 | `before_<event>`/`after_<event>` hooks for events declared only in `State(on={...})` | (b) | No naming-convention hooks. | ABSENT-DOC |
| F66 | A nested state class inheriting states/transitions from another | (b) | No nested classes. | ABSENT-DOC |
| F67 | The functional API mis-filed a callback decorated as another kind, duplicate names, nameless partials | (b) | Typed registration (`MachineLogic::dynamic().guard/action/service`) cannot mis-file. Duplicate names: last registration wins (registry), no scan. | ABSENT-DOC |
| F68 | `@action`/`@guard`/`@service` reject non-string/empty names and re-marking | (b) | No decorators. | ABSENT-DOC |
| F69 | A transition source/target `State` not in the tree but sharing a name bound to the last one | (b) | Targets are resolved by id/path at compile time (`Error::StateNotFound`), never by object identity. | ABSENT-DOC |
| F70 | `MachineBuilder`: `always=` leaking `""`, history+final/parallel, `child_states(initial=…, parallel=True)`, `root(id/states/initial)` | (b) | Rust `MachineBuilder` is a typed builder (`core/src/builder/*`); the JSON validator is the gate. | ABSENT-DOC (different builder) |
| F71 | `StateMachine(state="b")` warned "Lookup will fail" for a relative path | (b) | No façade; relative ids are accepted by `adopt::from_state_ids` (`a_leaf_names_its_ancestors_and_ids_may_be_relative`). | ABSENT-DOC |
| F72 | `SQLiteStore.load()` read the record and deadlines in two statements, so a concurrent save could tear them | (c) | `pers/src/sqlite_store.rs:479-524` reads both inside `in_read_tx` (`:436-448`, `BEGIN DEFERRED`), and the joined path only exists while `lock_file` holds `BEGIN IMMEDIATE` (`:685`), so both statements share one snapshot. | MATCH-T (`sqlite_store_round_trips_with_python`; race covered by construction) |
| F73 | `apersisted()` skipped a custom lock strategy's `acquire()` | (c) | No async `apersisted`; `persisted`/`LockStrategy` are generic and run the strategy (`locking.rs:426-520`). Stores are documented synchronous (guide:147-148). | ABSENT-DOC |
| F74 | `IdempotencyPlugin` admitted an event when the key/principal/instance key raised | (c) | `IdempotencyGuard` refuses on errors; `idempotency.rs::bad_keys_principals_and_ttls_are_errors`, `::a_dead_inbox_refuses_by_default_and_admits_on_request`. | MATCH-T |
| F75 | `flush_marks()` stopped at the first failing inbox write | (c) | The plugin plumbing/mark buffer is deliberately not ported; the caller holds a `Claim` and settles it (`idempotency.rs:26-31`). | ABSENT-DOC (rustdoc) |
| F76 | A non-list `pending_events`/`deferred` was a bare TypeError; a fractional snapshot version was truncated | (a) | `runtime_alignment::corrupt_event_records_refuse_the_restore`; `store_layouts.rs::a_fractional_record_version_is_corrupt_not_truncated`. | MATCH-T |
| F77 | `Deadline.remaining_ms()` raised OverflowError for a huge finite deadline | (c) | `scanner.rs::remaining_ms_clamps_a_huge_due_time_instead_of_overflowing`; `DeadlineExt` clamps to `MAX_REMAINING_MS`. | MATCH-T |
| F78 | `FileStore` leaked bare OSError; `delete`/`forget` did not take the key lock; `list_keys` advertised unloadable files; saves fsync the directory | (c) | `file_store.rs:749-769` (`list_keys` filters to loadable names), `:339-410` (delete/forget lock, stamp), `:145` (fsync dir on Unix); `store_layouts.rs::delete_waits_for_the_key_lock_so_a_save_cannot_resurrect_the_record`. | MATCH-T + CHANGELOG (Unix fsync branch never run on Linux — HANDOVER §5) |
| F79 | `MemoryStore.forget()` discarded a held lock | (c) | `memory_store.rs:268-275` (removes only when unheld/unawaited); `stores.rs::memory_forget_does_not_discard_a_held_lock`. | MATCH-T |
| F80 | `SQLiteStore` accepted any journal mode/busy timeout; file-creation errors; `close()` closes every thread's connection | (c) | `JournalMode::{Wal,Delete}` (`sqlite_store.rs:61-69`, removal documented in HANDOVER §5), `validate_busy_timeout` (`:176-189`), `store_layouts.rs:565-578`; no `close()`: connections live in the store and close on drop. | MATCH-T (journal/timeout) + ABSENT-DOC (`close`, **L5**) |
| F81 | `SQLiteInbox`/`SQLiteLog` leaked raw `sqlite3` errors | (c) | `idempotency/sqlite.rs:24` `sql_err`, `log/sqlite.rs:40-42` `insert_err`; `sqlite_log_uses_the_python_table_and_rejects_foreign_schemas`. | MATCH-T |
| F82 | `persisted()` exhausted a generator passed as `plugins`; no cleanup when `start()` raised | (c) | Observers are a `Vec` (`locking.rs:328`); `refuse_failed_start`; `persisted.rs::after_commit_callbacks_are_dropped_when_the_body_or_the_save_fails`, `::a_body_error_writes_nothing`. | MATCH-T |
| F83 | `load_interpreter` ignored restore options on a new key; unknown options; `expected_machine_hash` | (c) | `locking.rs:426-470` (hash checked against the machine on a new key and the snapshot on restore, forcing the drift check); `persisted.rs::expected_machine_hash_is_checked_against_the_machine_on_a_new_key`, `::expected_machine_hash_forces_the_drift_check_even_without_verify_hash`; struct options make unknown options unrepresentable. | MATCH-T + CHANGELOG |
| F84 | `DueTimerScanner.last_result` AttributeError; invalid `lock=` only at tick; `run_forever(0)` | (c) | `scanner.rs::last_result_exists_before_the_first_scan_and_follows_each_scan`, `::invalid_settings_are_refused_not_clamped`, `::a_zero_interval_is_refused_instead_of_spinning`; `DueTimerScanner::validate`. | MATCH-T + CHANGELOG |
| F85 | `TransitionLogPlugin` kept dropped events; `JSONLinesLog` re-validated a stale sequence cache | (c) | `TransitionLogObserver` records at settle; `log.rs:12-31` lists what is not ported; `jsonl_names_the_corrupt_line_and_never_skips_it`, `::jsonl_purge_rewrites_atomically_and_leaves_no_temp_file`. | EQUIVALENT (documented differences) |
| F86 | `replay_dead_letter` replayed a dead letter redacted at capture | (c) | `eda/tests/parity_0_12.rs::replay_refuses_a_dead_letter_redacted_at_capture_unless_forced` (sentinel as a whole leaf only; `force` overrides); CHANGELOG. | MATCH-T |
| F87 | `SseSink.close()` hung on a stalled client; `send()` recorded before encoding | (c) | `inspect/src/sse/mod.rs:271-273` encodes first; `inspect/tests/sse.rs::a_stalled_client_does_not_block_other_requests`, `::close_makes_later_sends_fail_and_frees_the_port`. | MATCH-T |
| F88 | `SseSink('::1')` bound IPv4-style | (c) | `inspect/tests/sse.rs::ipv6_loopback_binds_and_brackets_the_url`; `sse/auth.rs:22`. | MATCH-T |
| F89 | One corrupt SQLite outbox row stalled every later row | (c) | `eda/tests/outbox.rs::a_corrupt_sqlite_row_is_parked_and_later_rows_still_relay` (parked under `POISON_OWNER`); CHANGELOG. | MATCH-T |
| F90 | `OutboxRelay.relay_once` marked a row sent for a lost awaitable publish | (c) | Publish is a future that must be `.await`ed; `outbox.rs::relay_publishes_in_order_and_marks_sent`, `::a_failing_broker_marks_what_it_took_and_releases_the_rest`. | MATCH-T (structurally) |
| F91 | A state-store outage inside a run was dead-lettered as poison; a down DLQ double-counted | (c) | `parity_0_12.rs::a_state_store_outage_in_the_run_is_requeued_without_counting_an_attempt`, `::a_down_dead_letter_store_does_not_double_count_the_attempt`; `HandlerError::Requeue`. | MATCH-T + CHANGELOG |
| F92 | `Envelope.to_json` refused NaN/Infinity | (c) | `Value` cannot hold them; `parity_0_12.rs::a_non_finite_float_cannot_reach_the_wire_as_nan` (and reads refuse the tokens). | MATCH-T |
| F93 | `OutboxPlugin` validates `meta.publish` at start, warns on a missing `data` field, drains await | (c) | `outbox.rs::a_malformed_publish_spec_is_reported_not_swallowed`, `::a_publish_field_missing_from_the_context_is_published_as_null`; drain is synchronous (`after_commit`). | MATCH-T (sync analogue of "await all") |
| F94 | `InspectorPlugin` non-dict context; `read_jsonl` deep line; `OutboxRelay` batch < 1 | (c) | Context is `Value`; `inspect/src/sinks.rs:284` `an_absurdly_deep_line_is_an_error_not_a_stack_overflow`; `outbox.rs::a_zero_relay_batch_is_refused`. | MATCH-T |

### Fixed — CLI (14 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| F95 | `xsm generate-template` (all templates) wrote machine-supplied names into code unescaped | (b) | `generate-template` not ported (`guide:156-160`). | ABSENT-DOC |
| F96 | The generated runner is syntax-checked; `escape_for_string` escapes newlines/Unicode separators | (b) | `generate-template` not ported. | ABSENT-DOC |
| F97 | `xsm docs -o DIR` / `diagram -o DIR` built the file name from the raw machine id (`../escaped`) | (c) | `xsm/src/commands/model.rs:143-150` `file_stem_for` sanitises both commands; `models.rs::file_stem_neutralises_path_tricks`; `xsm/tests/cli.rs::diagram_never_lets_a_machine_id_escape_the_output_directory`; `docs.rs:182` uses the same helper. | MATCH-T |
| F98 | Function/class JSON logic stubs shadowed module names (`time`, `logging`, `Event`) | (b) | `generate-template` not ported. | ABSENT-DOC |
| F99 | `--json-child` without a parent; duplicate `--json-parent`; negative `--sleep-time` | (b) | `generate-template` not ported. | ABSENT-DOC |
| F100 | `generate-template` with existing files and closed stdin exited with EOFError | (b) | `generate-template` not ported. | ABSENT-DOC |
| F101 | Hierarchy runners did not bind logic to child machines | (b) | `generate-template` not ported. | ABSENT-DOC |
| F102 | Post-processing stripped `from __future__ import annotations` | (b) | `generate-template` not ported. | ABSENT-DOC |
| F103 | Multi-target transitions reported by name as unsupported by the code generator | (b) | `generate-template` not ported; `xsm inspect`/`diagram` handle every target (`cli.rs::inspect_docs_and_diagram_list_every_target_of_a_multi_target_transition`). | ABSENT-DOC |
| F104 | `xsm validate` reported an unreadable/non-UTF-8 file or unexpected shape as a finding | (c) | `xsm/src/commands/analysis.rs:238-260` (`read_to_string` failure → `Finding`); `cli.rs::validate_reports_a_missing_file_as_a_finding`, `::validate_reports_invalid_json_with_a_line_number`, `::validate_json_surfaces_the_engine_error_for_an_invalid_state_type_with_its_path`. | MATCH-T |
| F105 | Interactive simulator `t`/`c` keys (named delays, NaN/inf/negative) | (b) | No interactive TUI; `xsm simulate` is script/flag driven and rejects bad input with exit 2 (`cli.rs::simulate_refuses_bad_input_with_exit_two`). | ABSENT-DOC (HANDOVER §6.5 lists the TUI) |
| F106 | Interactive launcher message; simulate/pytest-scaffold logging state | (b) | No interactive launcher; pytest scaffolding not ported. | ABSENT-DOC |
| F107 | `xsm update --json` single document; rc/.dev/.post version parsing | (b) | `update` not ported. | ABSENT-DOC |
| F108 | `xsm gt --check/--diff --from-django-fsm` wrote the extracted chart | (b) | `generate-template`/Django not ported. | ABSENT-DOC |

### Fixed — web adapters (21 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| F109 | A resident that reached a final state was not saved when evicted/released/shut down | (c) | `xstate-axum` keeps no resident: each request is one `persisted` cycle that saves when the configuration/context changed (`registry/cycle.rs:405-435`), so a completion is saved like any other transition; `http.rs::no_op_send_is_not_saved` pins the inverse. No eviction path exists to lose it. | EQUIVALENT (architecture) |
| F110 | With `create_if_missing=False`, missing key → 404 and existing+unauthorised → 403 let callers enumerate keys; authorization now runs first | (c) | `routes/mod.rs:106` and `routes/ws.rs:130` authorize before existence; `http.rs::create_if_missing_false_answers_404` uses `allow_all` (so 404 is correct there); the unauthorized case is covered by the authorizer tests. | MATCH-T |
| F111 | Flask/Quart accepted a malformed/over-long `Idempotency-Key` and silently ignored it without an inbox | (c) | `http.rs::idempotency_key_rules` (401 anonymous, 400 malformed, 501 without an inbox); `body.rs:28-41`. | MATCH-T |
| F112 | A client-supplied `idempotency_key` in the JSON body bypassed header validation; now 422 | (c) | `body.rs:20` reserves only `wait`/`priority`; `cycle.rs:226-232` takes the key from the header and overwrites the payload field. No bypass (the guard is header-only), but no 422. | PARTIAL→**L3** |
| F113 | A GET on an unknown key through Flask/Quart started an interpreter and ran entry actions | (c) | `http.rs::get_reports_the_initial_state_without_saving` (peek, no save, no actions). | MATCH-T |
| F114 | Quart `GET /history` returned raw payloads and ignored `after`/`limit` | (b) | `xstate-axum` exposes no `/history` route (the transition log is the analogue, `TransitionLogObserver`/`SqliteLog`). | ABSENT-DOC |
| F115 | Litestar `get_interpreter` hung a sibling dependency | (b) | Litestar not ported. | ABSENT-DOC |
| F116 | A deeply nested JSON body/frame raised RecursionError → 500 | (c) | `body.rs:113-117`: serde_json depth limit → `UnprocessableBody` (422); WebSocket frames use the same body reader. | MATCH-T |
| F117 | Quart 5xx problem responses were not logged | (c) | `registry/cycle.rs:272-281` logs 5xx (and store unavailability at WARN); `routes/mod.rs` problem conversion. | MATCH-T (by code; no dedicated test) |
| F118 | Quart SSE parked a default-executor thread per client per heartbeat | (c) | SSE is `tokio` tasks + a bounded channel (`routes/stream.rs`, `fanout.rs`); `streaming.rs::sse_streams_snapshot_then_transition_and_heartbeats`, `::registry_wide_connection_cap_refuses_sse_and_websocket`. | EQUIVALENT |
| F119 | Quart body reads unbounded for chunked requests and had no timeout; 413/408 | (c) | `body.rs:75-95` `read_body` bounds every chunk (413) and wraps the read in `timeout` (408); `Content-Length` lies are irrelevant (streamed bound). | MATCH-T (`problem_shapes_for_reserved_keys_size_and_media_type`) |
| F120 | Flask SSE fan-out kept sequence counters for every key ever used and dropped for slow clients | (c) | `fanout.rs:10-12,51-122`: the counter exists only while a subscriber does; slow consumers are cut off (`publish_stamps_sequence_and_cuts_slow_consumers`). | MATCH-T |
| F121 | SSE/WebSocket read the snapshot before subscribing, losing a transition in between | (c) | Snapshot-then-stream is emitted from one subscription: `streaming.rs::sse_streams_snapshot_then_transition_and_heartbeats` (snapshot frame `id: 0`, then the transition `id: 1`); `ws.rs` subscribes before the snapshot. | MATCH-T |
| F122 | Flask `GET /<id>/history` with negative `after`/`limit` answered 500 | (b) | No history route. | ABSENT-DOC |
| F123 | An event to a finished instance answered 500 (now 409); an oversized snapshot answered 500 (now 413) | (c) | `http.rs::guard_refusal_is_denied_409_and_finished_instance_is_409`; `::problem_shapes_for_reserved_keys_size_and_media_type` (413). | MATCH-T |
| F124 | Numeric registry options validated at construction | (c) | `http.rs::bad_router_prefix_and_zero_body_limit_are_setting_errors` (`max_body_bytes(0)`, `max_connections(0)`); `RegistryError::InvalidSetting`; `registry.rs:195-290`. The Rust option set differs: there is no resident idle TTL because there is no resident cache; `run_timers`/`drain_timeout` are builder settings (`registry.rs:256-290`). | MATCH-T |
| F125 | Flask/Quart `act()` from the caller's view answered 500 for an anonymous caller | (b) | `xstate-axum` `act()` is an application API without HTTP principal semantics (`guide`); client entry points answer 401 (`idempotency_key_rules`). | ABSENT-DOC |
| F126 | Origin check compared host names case-sensitively and had two copies | (c) | One shared copy (`registry.rs:514-527`), but still case-sensitive. | PARTIAL→**M1** |
| F127 | The flask `xsm` CLI turned `sys.exit()` into exit 1 | (b) | No flask blueprint CLI. | ABSENT-DOC |
| F128 | Event names unsafe in a URL path fail the router build | (c) | `routes/mod.rs:118-121,215-217`: a non-routable event is skipped with a warning and served only by `/send` (deliberate, non-panicking; the alternative is a build error). | EQUIVALENT (differs from Python's ValueError; undocumented in the guide) |
| F129 | `instrument_app` deduplicated by registry identity; probes mounted ahead of catch-alls; 5xx logged | (c) | `probes_router()` is separate and mounted by the caller (`lifecycle.rs::probes_follow_the_lifecycle`); no app-mounting helper exists. | ABSENT-DOC (explicit mounting) |

### Fixed — Django & SQLAlchemy (14 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| F130 | A dotted `statechart_machine`/`statechart_logic` callable taking the row was resolved once with `row=None` and cached | (b) | Django not ported (`guide:44-46`). | ABSENT-DOC |
| F131 | `StatechartModelMixin` with a prebuilt `MachineNode` dropped the logic; SQLAlchemy mixin type checks | (b) | Django/SQLAlchemy not ported; `adopt` is the generic row-adoption primitive (A14). | ABSENT-DOC |
| F132 | DRF `history/` 500 for UUID/str user PKs | (b) | Django/DRF not ported. | ABSENT-DOC |
| F133 | DRF send / Channels consumer 5xx not logged | (b) | Not ported; `xstate-axum` logs 5xx. | ABSENT-DOC |
| F134 | `DjangoStore` classified any message containing "locked"/"busy" as contention | (c) | `PostgresStore` maps SQLSTATE codes (`postgres_backend/db.rs`), not message text, so a table named `locked_orders` cannot be misclassified; `driver_errors_never_carry_the_host_or_the_password`. | EQUIVALENT (better) |
| F135 | `AsyncSQLAlchemyStore` leaked driver errors and skipped the schema check with `create_tables=False` | (c) | `PostgresStore` verifies/upgrades the schema (`store_layouts.rs::the_schema_marker_is_checked_and_upgraded`) and maps driver errors (`postgres_backend/db.rs`). | EQUIVALENT |
| F136 | `FSMDualWriteMixin` wrote the legacy column from the receipt, missing engine steps | (b) | No mixin; `adopt::from_state_ids` and `Snapshot::from_state` derive from the written snapshot, which includes engine steps. | ABSENT-DOC (primitive exists) |
| F137 | A wide parallel chart overflowing `<name>_state` raised a clear ValidationError | (b) | No state column; state ids are JSON. | ABSENT-DOC |
| F138 | Django admin `render_change_form` index; bulk action aborting on one row | (b) | Django not ported. | ABSENT-DOC |
| F139 | `SQLAlchemyMixin.send` accepts `payload=`; exports `RESERVED_PAYLOAD_KEYS` | (b) | `xstate-axum` `RESERVED_SEND_KEYS` (`body.rs:20`) is the HTTP analogue; not a mixin API. | ABSENT-DOC |
| F140 | Concurrent first saves could raise a raw IntegrityError | (c) | `stores.rs::creating_a_key_twice_concurrently_conflicts_on_version_zero`, `::concurrent_creators_have_exactly_one_winner_and_upserts_never_repeat_a_version`; `postgres_backend/store.rs`. | MATCH-T |
| F141 | `xsm_deadlines --forever` never refreshed connections and exited on a transient error | (b) | Django management command not ported; `DueTimerScanner::run_until` is the Rust loop (`scanner.rs::run_until_scans_repeatedly_and_stops_when_asked`). | ABSENT-DOC |
| F142 | SQLAlchemy `ModelStore.forget()` left the snapshot; malformed keys meant not-found | (c) | `persisted.rs`/`stores.rs::forget_erases_everything_about_a_key_but_inbox_rows`, `::invalid_keys_are_rejected_by_the_store`; `forget` removes record/deadlines/lease/log. | MATCH-T |
| F143 | Channels consumer survived a deleted row; refused oversized frames; cancellation | (b) | Channels not ported; `xstate-axum` WebSocket refuses oversized frames/events (`streaming.rs`). | ABSENT-DOC |

### Fixed — messaging & agents (8 items)

| ID | Python 0.12.0 item | Class | Rust location / test | Verdict |
|---|---|---|---|---|
| F144 | `RedisStore.list_keys` advertised expired keys and the index grew without bound | (c) | `pers/src/redis_backend/store.rs:261-278` `live_keys` prunes and filters; `redis.rs::ttl_expires_snapshots_and_due_keys_prunes_the_orphans`; HANDOVER §5 notes the set is pruned on `list_keys`/`forget`. | MATCH-T |
| F145 | RabbitMQ stopped consuming after a failed ack because the un-acked counter never recovered | (c) | `eda/src/rabbitmq/transport.rs:366-370,374-377`: `release()` is skipped when the settle call fails. | **GAP→H3** |
| F146 | A failed broker `ack`/`nack(requeue=False)` left the delivery un-retriable; Redis Streams hid it after a failed XACK | (c) | Redis Streams releases the held entry regardless of XACK (`redis_streams/transport.rs:146-153`). RabbitMQ leaks the unacked slot (H3), and the async core removes the delivery from `inflight` before the transport call (`async_core.rs:290-295`), so a retry is a silent no-op. | PARTIAL→H3 (Redis MATCH-T: `redis_streams.rs`) |
| F147 | `PydanticCodec` rejected library-private `_xsm_*` keys | (b) | Pydantic not ported. | ABSENT-DOC |
| F148 | The pydantic config gate accepted lax `"false"` for `internal`/`reenter` | (b) | Pydantic not ported; the engine's own parser is strict (C34). | ABSENT-DOC |
| F149 | A NaN/inf/garbage usage block made the agent turn retry; now fails closed on the budget | (c) | `agents/src/budgets.rs:6,13,144-151` (`spent_tokens`/`spent_cost` fail closed); `agents/tests/agent.rs::budget_plugin_rollup_sanitises_reports`, `::budget_exhaustion_matches_python`. | MATCH-T |
| F150 | Agent tools taking `datetime`/`Decimal`/`UUID`/enum could not be called; sync tools blocked shutdown | (b) | Rust tools are JSON-schema-based (`xstate-agents` tool registry); there is no Python type system and tools run on the async runtime (sync tools are inline). | ABSENT-DOC (different type model) |
| F151 | `RedisStore` errors included the connection string; a colon-only prefix was accepted | (c) | `redis.rs::driver_errors_never_carry_the_address`, `::driver_errors_never_carry_the_host_or_the_password`; `redis_backend/keys.rs:47-64` rejects a colon-only prefix. | MATCH-T |

## Verified sound

The following were checked and found correct; they are the load-bearing parts of the alignment:

1. **Every engine-semantics bullet (C1–C18)** is pinned by a conformance case whose recorded
   Python 0.12.1 expectation is identical to the XState 5.33.3 one, and whose Rust trace the
   suite asserts (`crates/xstate-conformance/tests/engine.rs:33`): cases `020`, `021`, `022`,
   `023`, `024`, `027`–`032`, `038`–`053`. `generate_expected.py --check` → 53 cases, 0
   divergent, 0 stale.
2. **Python snapshot/hash compatibility**: `snapshot.rs:67` (hash), `:94` (values), `:172`
   (text), `:213` (Python text restores), all on the same 53 fixtures.
3. **Config validation (C32–C40, F9/F45/F46/F50/C21)**: `core/src/config/parse/tests.rs`
   covers state type, history, boolean flags, error paths, action objects, invoke fields, empty
   targets, root numbers, `initial` hint, nesting; `alignment.rs` covers `after` normalisation,
   user-action-over-built-in, dotted ids, empty events; `runtime_alignment.rs` covers corrupt
   event records.
4. **Engine error/runtime rules (C49, F15–F18, F21–F26, F37, F42, F47–F52)**: the alignment
   suites pin unresolved targets, nested `choose` failures, final-entry failures, unusable
   delays, rollback history, taken-at, non-finite clocks, pure transitions; `eventless.rs`
   pins entry-order and once-per-macrostep settling.
5. **Persistence (F72–F85, F140, F142)**: tests named in the table, plus cross-language
   store-layout tests (`cross_language.rs`) and `the_tables_either_runtime_creates_are_the_same_tables`.
6. **EDA (F86–F94, F144, F146, F151)**: `eda/tests/parity_0_12.rs`, `outbox.rs`, `dead_letter.rs`,
   `redis_streams.rs`, `rabbitmq.rs`, plus the Python-golden tests.
7. **CLI analogues (F97, F104)**: `xsm` path sanitisation, validation findings, replay/inspect
   multi-target, stale-by-hash.
8. **HTTP analogues (F109–F111, F113, F116, F119–F121, F123, F124)**: named axum tests.
9. **Redaction rule shape (F55) in the core redactor**: the snake_case normalisation, camelCase
   split, dash handling and short-whole-word/long-substring split match Python exactly for the
   entries both lists share (verified with a Python snippet: `shipping`/`company`/`author`/
   `spinner` untouched; `user_pin`/`userPin`/`cardNumber`/`sessionId` masked). The **list
   contents** and the transition-log copy are H1.
10. **Python-only `maxIterations: null` and `spawnBlockingTimeout: null`**: Python refuses null
    `maxIterations` and accepts null `spawnBlockingTimeout`; Rust does the same
    (`config/parse/tests.rs:184`, `config/parse.rs:299-301`) — checked with a Python snippet.
11. **Deliberate differences are documented where they exist**: `invoke` without `input` → `null`
    (guide:77), event spellings (guide:76), multi-target in diagrams (guide:78), actor addressing
    (guide:111-114), restore-resumes-timers (guide:135-140), async `run_once` batching
    (guide:141-146), synchronous stores (guide:147-148), CLI subset (guide:156-160),
    `maxIterations` string (guide:105-106), built-in `params` with logic (guide:96-100),
    unusable delays (guide:101-104).

## Could not resolve here

- **No cargo run from this section** (shared `target/` owned by the §6.1 builds agent). The
  committed tests and the previous baseline log were used; the newest commits (`b0981e3`,
  `290f83a`, `903077c`) touch only nesting/source-reparse/fast-path gating, none of the
  alignment behaviours.
- **Docker-backed cross-language tests** (SQLite/Redis/PostgreSQL stores, Redis Streams, NATS,
  RabbitMQ, SQS) were not re-run; the items above are judged from the committed tests and code.
- **F109/F118/F120/F128** were assessed from the Rust architecture rather than by running the
  Python web adapters, which are not ported.
- The Python check was run against **0.12.1**, the installed release. The Python repo's own
  `[Unreleased]`/0.12.1 entry says the only library change after 0.12.0 is the FileStore stamp,
  so this is the right oracle for 0.12.0 semantics; if the owner wants a literal 0.12.0 run, the
  venv would have to be rebuilt at the tag.
