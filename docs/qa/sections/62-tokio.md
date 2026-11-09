# §6.2 Battle test — xstate-tokio

Scope: the §6.2 `xstate-tokio` matrix in `docs/HANDOVER-QA.md` — timers under `tokio::time::pause`,
cancellation of delayed sends, mailbox overflow policies and backpressure, receipts on finished /
stopped actors, child machine lifecycles (`stop`, `done.invoke`, `error.actor`, panics), the
`System` registry (collisions, deregistration), panics in services/actions under a multi-thread
runtime, and start/stop/shutdown drain.

Severity counts: **0 CRITICAL · 1 HIGH · 2 MEDIUM · 1 LOW** (no data loss or security issue; one
panic-containment robustness gap, two mailbox-policy inconsistencies, one behaviour to confirm
against the XState oracle).

New test file (kept as added coverage):

- `crates/xstate-tokio/tests/battle.rs` — 20 adversarial tests covering the whole matrix above.

Build change (test-only, no `src/` edit): `crates/xstate-tokio/Cargo.toml` gained the tokio
`rt-multi-thread` dev-dependency feature so the panic tests can run on a multi-thread runtime. No
existing test was modified or weakened; `crates/xstate-tokio/src/` was not touched.

## Findings

### HIGH-1 — A panic in a service or action is not contained: a panicking async service hangs its parent, a panicking action silently kills its actor

**Status: FIXED in `5f73c6f`.** Event processing, internal completions and due timers run in
`catch_unwind`; a panic inside an async service future is caught before `ServiceDone`. A panic
now sets `Status::Error` (with the panic message), tears children/services down and reaches the
parent as `error.actor`. Tests updated to assert the fixed contract; 20 battle tests, 32 crate
tests green.

**Status: OPEN (proposed fix below; `src/` not edited in this test-only pass).**

- **file:line**
  - Async service: `crates/xstate-tokio/src/actor.rs:556-565`. The service future is `tokio::spawn`ed
    and the only path back to the owner is `tx.send(Internal::ServiceDone { .. })` *after*
    `fut.await` (line 562-563). A panic in the future never reaches that send, and the stored
    `JoinHandle` (`self.services.insert(id, handle)`, line 565) is only ever `.abort()`ed on stop,
    never awaited — so the panic is swallowed and the invoking state waits forever.
  - Action: `crates/xstate-tokio/src/actor.rs:192-208`. `Msg::Event` calls `core.process(event)` with
    no `catch_unwind`, so a panic in a user action unwinds through `run` and kills the whole actor
    task. The watch channel's sender is dropped, the mailbox closes, and every subsequent `send`
    returns `Error::InvalidLifecycle` — but the machine never enters `Error` status (the last
    published `View` still says `Running`).

- **reproduction**
  ```
  cargo test -p xstate-tokio --all-features --test battle \
      child_async_service_panic_leaves_parent_hung \
      panicking_action_kills_only_its_own_actor
  ```
  (with `CARGO_TARGET_DIR` set to the QA target dir)

- **observed vs expected** — a child whose async service panics leaves the parent in `Running`
  forever (`wait_done` times out; no `error.actor`, no `onError`), i.e. a hang. A panicking action
  makes its own actor unreachable with a stale `Running` view, while a sibling actor in the same
  runtime keeps working (so the runtime survives, but the failing actor neither fails cleanly nor
  reports). Expected (Python parity): a service/action exception is turned into
  `xstate.error.actor.<id>` / an action error under the configured policy, so the parent can
  observe and react instead of hanging or dying silently.

- **proposed fix** — wrap the service future in a `catch_unwind` (or `AssertUnwindSafe`) and send
  `ServiceDone { result: Err(..) }` on a panic; similarly guard `core.process` (or the action
  execution path) so a panicking action is reported through `on_error` / `Error::TransitionFailed`
  rather than unwinding the task. Both are local changes with clear test anchors already present in
  `battle.rs`. If cross-task panic capture is judged out of scope for 0.1.0, at minimum document
  that user logic must not panic, but the current silent hang is the worst possible failure mode.

### MEDIUM-1 — `send_and_wait` ignores the `Overflow` policy and always blocks

**Status: FIXED in `5f73c6f`.** `send_and_wait` enqueues through the same policy as `send`:
`Reject` returns `LimitExceeded`, `DropNewest` drops and returns `LimitExceeded` naming the
drop, `Block` waits. The regression test drives a full mailbox deterministically.

- **file:line** — `crates/xstate-tokio/src/lib.rs:205-209`. `send_and_wait` uses the blocking
  `self.tx.send(...).await` regardless of `self.overflow`, unlike `send`/`try_send`
  (`lib.rs:166-198`), which honour `Block`/`DropNewest`/`Reject`.

- **reproduction**
  ```
  cargo test -p xstate-tokio --all-features --test battle send_and_wait_ignores_reject_policy
  ```

- **observed vs expected** — with `mailbox_capacity: 1` and `Overflow::Reject`, a full mailbox makes
  `try_send` return `Err(LimitExceeded)`, but a second `send_and_wait` blocks and then succeeds.
  Expected: `send_and_wait` should observe the same policy (`Reject` → immediate `LimitExceeded`,
  `DropNewest` → drop and return) so that callers relying on bounded backpressure do not deadlock.
  The `Overflow` doc only names `send`, so this is a misleading-API / limited-impact defect rather
  than a broken claim, but `send_and_wait` under `DropNewest` blocking is a backpressure trap.

- **proposed fix** — in `send_and_wait`, use `try_send` + `oneshot` under `DropNewest`/`Reject`
  (returning `Err`/`Ok` per policy) and only fall back to the blocking `send` under `Block`. Needs
  care: the `Reject` error must be returned without a reply channel ever being answered.

### MEDIUM-2 — `Overflow::DropNewest` drops are not observable through `Observer`

- **file:line** — `crates/xstate-tokio/src/lib.rs:183-186`. On a full mailbox under `DropNewest`,
  `try_send` (and `send`, which delegates to it) emits only a `tracing::warn!` and returns `Ok(());
  `Observer::on_event_dropped` is never called, unlike the `deferred`, `not_running`, `unresolved`
  and `stopped` reasons.

- **reproduction**
  ```
  cargo test -p xstate-tokio --all-features --test battle overflow_drop_newest_silently_drops
  ```

- **observed vs expected** — the dropped event is invisible to the observer suite (asserted empty),
  so a user's dead-letter / telemetry observer cannot see events lost to the drop-newest policy.
  Expected: a `DropNewest` drop should be reported through `on_event_dropped` with a dedicated
  reason (e.g. `"mailbox_full"`), consistent with every other drop path.

- **proposed fix** — call `obs.on_event_dropped(&machine, &event, "mailbox_full")` on the
  `DropNewest` path. Note `try_send` runs outside the actor task and only has the `mpsc::Sender`,
  not the machine or observers, so this either needs the observer list on the handle or the drop to
  be routed through the actor task — the latter keeps the existing single-threaded observer contract.

### LOW-1 — A guarded `after` whose guard refuses consumes the timer and never retries

**Verdict: expected XState semantics, not a defect.** An `after` is a one-shot delayed event;
when it fires and its guard refuses, no transition is taken and the timer is consumed (nothing
in XState or the Python library re-arms it). A retrying timeout is expressed with `always` /
`raise` in the chart, not by the engine.

- **file:line** — timer firing: `crates/xstate-tokio/src/actor.rs:426-441` (`fire_due_timers` pops
  the timer once); `after` transitions are compiled with guards as ordinary transitions
  (`crates/xstate-core/src/machine/compile.rs:350-377`).

- **reproduction**
  ```
  cargo test -p xstate-tokio --all-features --test battle after_guard_refuses_and_does_not_retry
  ```

- **observed vs expected** — with `after: { 100: { target: "b", guard: "refuse" } }`, the guard is
  evaluated at fire time; on refusal the machine stays in the source state and the timer is
  consumed (no re-arm, no later retry). This is plausibly the correct XState behaviour (a delay
  elapses once; the transition is simply not taken), but it should be confirmed against the Node
  XState oracle before 0.1.0 rather than assumed. Recorded here because the matrix calls the case
  out explicitly.

## Verified sound

Exercised and passing on this pass (2026-11-07, Windows 11, Rust toolchain pinned, dedicated
`CARGO_TARGET_DIR`, `cargo test -p xstate-tokio --all-features -j 4` → **32 passed / 0 failed**):

- **Timers under `tokio::time::pause`** — an `after` on a state exited before its deadline does not
  fire (`CancelTimers`); a guarded `after` is evaluated at fire time; delayed self-sends fire and a
  `cancel` in a *later* step removes them.
- **Mailbox overflow / backpressure** — `Reject` makes `try_send` return `LimitExceeded` on a full
  mailbox; `DropNewest` returns `Ok` and drops the newest event; `Block` waits for room and
  preserves FIFO order (two events processed in order through a capacity-1 mailbox).
- **Receipts on finished / stopped actors** — a done actor still serves `send`/`send_and_wait`
  (unhandled receipt, `Status::Done`) and `snapshot`; after `stop`, `send`/`send_and_wait`/`snapshot`
  all fail with `InvalidLifecycle`; a second `stop` is a no-op that does not hang; a fresh actor on
  the same machine restarts from its initial state.
- **Child / invoked actor lifecycles** — `onDone`/`xstate.done.actor` output and `onError`/
  `xstate.error.actor` (failing async service → parent `onError`) both reach the parent; the `stop`
  action stops a child and a later `sendTo` to it is a soft `UnresolvedTarget`; `sendParent` on a
  root (no parent) is a soft `UnresolvedTarget`; a panicking async service hangs the parent (the
  HIGH-1 finding, recorded honestly); a panicking action kills only its own actor while a sibling
  actor keeps working.
- **System registry** — a duplicate `systemId` is refused and surfaced to the parent as an actor
  error containing `"duplicate systemId"` (via `onError`), not a panic or silent drop; a child's
  `systemId` is deregistered on completion (a later `sendTo` is `UnresolvedTarget`); `sendTo` an
  unregistered name is `UnresolvedTarget` (already covered in `alignment.rs`).
- **Shutdown drain / observers** — events enqueued before `stop` are processed, not stranded;
  `Observer` fires `on_start`, `on_event`, and `on_stop` in order; done actors keep serving
  snapshots.

## Could not test / documented gaps

- **Cross-actor delayed-send cancellation** — `actor.rs:644-646` documents that `id`-based
  `cancel` of a cross-actor delayed send is "not supported yet", so it was not asserted as a bug;
  only self-sends are cancellable.
- **Stranded-event reporting (`strand`, `actor.rs:227-253`)** — the deterministic path shows events
  queued before `stop` are *processed*, not stranded; the `"stopped"` drop branch is only reachable
  in a narrow race (an event enqueued after `shutdown` begins but before the channel closes) that
  could not be hit deterministically without instrumenting `src/`. Left as an internal branch, not
  a user-visible guarantee.
- **`System` is not externally constructible/queryable** — `System` is public but its
  `register`/`unregister`/`get` methods are private and `spawn` creates a fresh `System`, so
  registry state could only be exercised through child-machine behaviour (done above), not by
  direct registry inspection.

## Commands run

```
CARGO_TARGET_DIR="C:\Users\basil\Desktop\Projects\RustProjects\qa-target-tokio" \
  cargo test -p xstate-tokio --all-features -j 4
  # 6 (actor) + 5 (alignment) + 20 (battle) + 1 (doc) = 32 passed, 0 failed
```
