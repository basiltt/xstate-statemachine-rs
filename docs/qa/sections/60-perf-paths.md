# §6.0 Performance-pass audit: fast path, direct compiler, bitset, registry, text, restore

Severity tally: CRITICAL 0 · HIGH 1 · MEDIUM 1 · LOW 2. All items are report-only; no repo
code was changed by this section (the scratch evidence copy under `docs/qa/work/audit-core/`
must not ship; it is untracked scratch).

## Method and evidence boundary

Every hot-path piece named in `docs/HANDOVER-QA.md` §6.0 was read in full: the plain-microstep
fast path (`engine/algorithm.rs`, `engine/sets.rs` `Plans::is_plain` + `exit_range`), the direct
compiler (`machine/direct.rs`, `assemble.rs`, `compile.rs`, `source.rs`, `builtin.rs`), the
bitset (`engine/bits.rs`), selection (`engine/select.rs`), the logic registry
(`logic/registry.rs`), the interned text helpers (`text.rs`), the lazy `CancelTimers` /
restore path (`interpreter/restore.rs`), and the `OnceLock` caches and `ClockHandle`
(`machine/mod.rs`, `interpreter.rs`).

Empirical work ran in an isolated copy of `xstate-core` at
`docs/qa/work/audit-core` (standalone manifest, `CARGO_TARGET_DIR=…\qa-target-perf`, so the
shared workspace `target/` was never contended). Tests added there, never to the repo tree:

- `engine::qa_audit::qa_fast_path_adversarial_matrix` — 17 hand-built machines covering
  structures the random generator never produces (root handlers, root `after` / entry / exit,
  `after` on exited/entered states, invokes, finals, shallow + targeted history, parallel
  with `initial`, compound self-targets, inactive states with `after`/exit actions inside the
  exit range), each with its plain-qualification asserted and its fast/general traces compared
  (receipts, configuration, context, history, queue, `mutations`, `entry_seq`, `entered_at`).
- `machine::direct_tests::qa_adversarial_documents_compile_identically_both_ways` — 34 new
  documents at the edges of the direct path's acceptance (guard strings `and`/`or`/`stateIn`,
  built-in-spelled string actions, engine-named descriptors, `after` keys `" 100 "`, `"inf"`,
  `"NaN"`, `"1_0"`, `9999999999999999`, numeric `version`, scalar `context`, unicode ids,
  `always: null`, empty `on` descriptor, transition `x-`/`meta` keys, policies/strictConfig,
  `onDone`/invoke/output/meta/tags fallbacks, duplicate custom ids), each compared with the
  parser path on the full fingerprint, `to_json`, structure hash, warnings and identical
  error strings, and with its direct/fallback expectation asserted.
- Mutation testing of the fast-path bookkeeping in the copy: mutations B (exit bits not
  cleared), C (entry stamps refreshed even when preserved) and D (`mutations` bumped on every
  plain microstep) each fail `fast_path_matches_general_path` within a few cases
  (< 0.3 s at 600 cases). Mutation A (`report.changed |= exited > 0` removed) passes even
  20 000 proptest cases plus the full 17-machine matrix: see L2.
- `qa_deep_nesting_both_paths` / `qa_probe_overflow_threshold` — programmatically built
  Values at 30–1600 state levels, both compile paths, `to_json` and structure hash.

The 100 000-case proptest run of `fast_path_matches_general_path` at HEAD is the prior
session's (`docs/qa/work/proptest_fast_path_100k.log`, 334 s, pass); this audit re-ran 10 000
and 20 000 cases in the copy (pass, 21 s and 35 s).

## Findings

### HIGH-3: the parser path aborts the process (stack overflow) on deeply nested programmatic input

**Status: FIXED in `b0981e3`.** `MAX_STATE_NESTING = 128` (root 0) is enforced by the parser
(`Error::LimitExceeded`, checked before recursing), the direct compiler (returns `None`, so the
parser produces the error), and `machine::compile::compile` (an iterative check, so a hand-built
`MachineConfig` — every field is public — cannot overflow `count_states`/`flatten` either).
Tests: `state_nesting_is_bounded` (128 accepted on both paths, 129 refused with the typed error)
and `hand_built_config_nesting_is_bounded`. Verified with 44 core unit tests, conformance both
feature ways, workspace clippy `-D warnings`, rustdoc and cargo-deny.

- **Where:** `crates/xstate-core/src/config/parse.rs` `Parser::parse_state` (recursive, no
  depth bound); `crates/xstate-core/src/machine/direct.rs` `Flat::state` (recursive, no depth
  bound — but survives the same inputs, so the parser path fails first).
- **What:** a `Value` built programmatically and nested ~300–400 state levels deep overflows
  the stack during `parse_without_source` (exit `0xc00000fd`, STATUS_STACK_OVERFLOW) — an
  uncatchable process abort, worse than a panic. `Machine::from_value` and
  `Machine::from_config` are public and reachable with such input (a `Value` can come from a
  caller-configured deserializer with the recursion limit disabled, or be built in code).
- **Not reachable from untrusted JSON text:** `serde_json::from_str` enforces a 128-level
  recursion limit, so `Machine::from_json`, snapshots and envelopes reject deeper text before
  the compiler sees it. The exposure is the value/config API, not the wire.
- **Repro:** `docs/qa/work/audit-core`, test `qa_probe_overflow_threshold` — prints
  `depth 400: parser path`, then the thread aborts (stack overflow). 300 levels pass; the
  direct path passes 1600+.
- **Fix (proposal for the owner):** enforce a maximum state-nesting depth in *both* paths
  with a typed error (`Error::LimitExceeded("machine nesting exceeds N levels")`) before
  recursing; a cap of e.g. 128 state levels keeps the parser comfortably within any thread
  stack while staying above serde_json's own 128-JSON-level cap (which counts every wrapper,
  so 128 *state* levels is far beyond what untrusted JSON can ever supply). Add a test at the
  boundary and just below it. The direct path should apply the same bound so the two paths
  stay equivalent.

### M1 (MEDIUM): `to_json()` silently returns `null` for values nested past serde_json's reparse limit

**Status: FIXED in `290f83a`.** `Source::to_value` reparses the stored text with
`Deserializer::disable_recursion_limit()` (the text is this process's own compact encoding;
caller-supplied JSON still parses under the default limit). `serde_json` gains the
`unbounded_depth` feature, which only makes the method callable. Regression test:
`deep_documents_round_trip_past_the_reparse_limit` — it asserts the document really is past the
default limit before the round-trip.

- **Where:** `crates/xstate-core/src/machine/source.rs:63-67` (`Source::Json(text) =>
  serde_json::from_str(text).unwrap_or(Value::Null)`).
- **What:** `Source::from_value` serialises the input to compact JSON text; `to_value` then
  re-parses it with `serde_json::from_str` and its default 128-level recursion limit, and
  swallows the failure as `Value::Null`. A machine built via `Machine::from_value` from a
  programmatically nested value (~40+ state levels once the JSON wrappers count) therefore
  returns `null` from `to_json()` / `to_json_string()` even though it compiled and runs.
  The doc contract on `Machine::to_json` ("the input verbatim") silently breaks, and any
  downstream consumer of `to_json` (exporters, tools) sees `null`.
- **Repro:** `qa_deep_nesting_both_paths` in the scratch copy: depth 30 round-trips, depths
  64/128/256 return `Value::Null` (asserted as the recorded defect). The compiled machine and
  both paths' structure hashes are fine — only the source round-trip is broken.
- **Fix (proposal):** parse the stored text back with the recursion limit disabled
  (`serde_json::Deserializer::from_str(&mut de).disable_recursion_limit()`); the text was
  produced by `serde_json::to_writer`, so it is trusted, and disabling the limit there
  re-introduces no untrusted-input surface. Alternative: keep `Source::Document(value.clone())`
  when serialising would exceed the reparse depth (measure depth while writing). Add a
  regression test with a programmatically nested value.

### M4 (MEDIUM): the mandated "conformance on the general path" run is not executable — design proposed

**Status: FIXED in `903077c`.** The sketch below was implemented as written:
`xstate-core` has `default = ["fast-path"]`, `fast_path_enabled()` is `cfg!(feature =
"fast-path")` outside tests, and `xstate-conformance` forwards the feature with
`xstate-core = { path = ..., default-features = false }` (spelled out because cargo 1.88, the
MSRV, rejects a member's `default-features = false` for a workspace dependency that does not
declare it). `cargo test -p xstate-conformance --no-default-features` passes all 53 cases, the
snapshot vectors and the fixture checks, and CI runs it in the `test` job. Feature resolution was
verified in the rustc invocation (`feature="fast-path"` present by default, absent with
`--no-default-features`).

- This is finding M1 of `sections/60-commits.md`, re-verified and solved here. The only
  fast-path switch is the `cfg(test)` thread-local in `engine/algorithm.rs:188-202`, so a
  separate crate (`xstate-conformance`) cannot disable it.
- **Recommended fix (exact patch sketch):**
  1. `crates/xstate-core/Cargo.toml`: `[features] default = ["fast-path"] / fast-path = []`.
  2. `engine/algorithm.rs`: replace the `cfg(not(test))` body with `cfg!(feature = "fast-path")`
     (a compile-time constant, so non-test builds still fold the branch away; the `cfg(test)`
     per-thread toggle is unchanged for the differential).
  3. `crates/xstate-conformance/Cargo.toml`: `default = ["fast-path"]`,
     `fast-path = ["xstate-core/fast-path"]`.
  4. Run the whole 53-case suite and every golden with
     `cargo test -p xstate-conformance --no-default-features`; add that command to the CI
     matrix and to `conformance/README.md`.
- Rejected alternatives: an env-var switch (adds a process-global branch to the hot path and
  makes behaviour environment-dependent); duplicating the 53-case runner inside `xstate-core`
  (test-logic duplication, and the goldens/parser fixtures live in the conformance crate).

### L2 (LOW): `report.changed |= exited > 0` in `plain_microstep` is dead weight

- **Where:** `engine/algorithm.rs` plain-microstep body, the exit-set block.
- **What:** on the fast path, a transition with a non-empty exit set always has a non-empty
  entry set (the entry set contains each target or its initial chain; history-free machines
  have no plans otherwise), so the later `report.changed |= !entry.is_empty()` already sets
  the flag. Removing the clause was unobservable at 20 000 proptest cases plus the full
  17-machine adversarial matrix (mutation A), confirming it is redundant rather than a test
  gap. Harmless; keep or drop during the next pass.

### L3 (LOW): `qa_audit` scratch evidence exists only in this report's copy

**Status: FIXED in `5a3e1c2`.** The 17-machine matrix now lives in
`crates/xstate-core/src/engine/qa_audit.rs`, and the 34 adversarial direct-compiler documents in
`machine/direct_tests.rs`; both run with `cargo test -p xstate-core --lib` (46 tests pass). The
diagnostic probes (stack-overflow threshold, `always` spellings, deep-nesting round-trip) were
not promoted — the first two print only, and the third's expectations describe the pre-fix
behaviour; the boundary is now covered by `state_nesting_is_bounded`.

- The 17-machine matrix and the 34 adversarial direct-compiler documents found no defect and
  materially extend coverage of both equivalence tests. Recommend promoting them (trimmed of
  probes) into `engine/fast_path_tests.rs` and `machine/direct_tests.rs` in a `test:` commit
  so they run in CI; they cost < 10 ms combined.

## Verified sound

- **Fast path (plain microsteps).** Bookkeeping equivalence verified field by field against
  the general `microstep`: report microsteps/transitions, exit-range bit clearing vs
  `compute_exit_set`, positional `preserved` equality vs set equality, entry stamps and
  `entry_seq` (root excluded), `mutations` under `note_snapshot_change` (event microsteps
  never carry a `context_before`), effect ordering (`run_actions` is the shared function;
  plain steps have no exit/entry effects), and `exit_all_when_done` is a provable no-op on
  the fast path (plain transitions never enter finals, targetless ones never change the
  configuration). The qualification gate in `Plans::is_plain` is conservative: it checks
  every state in the exit *range*, active or not, and excludes `after`/invokes/finals from
  entry sets; history machines get no `Plans` at all. All 17 adversarial machines agree
  fast-vs-general, including root handlers (domain = root, root itself neither exited nor
  re-entered — matching XState's `getTransitionDomain`), compound self-targets, and
  preserved re-entry.
- **Mutation sensitivity.** B/C/D each fail the differential in < 0.3 s (600 cases);
  A is provably unobservable (L2), not a missed test.
- **Direct compiler.** Acceptance is a strict subset of the parser's: every accepted key is
  in the parser's `STATE_KEYS`/`ROOT_ONLY_KEYS`; `kind_of` / `initial_of` mirror
  `parse_kind` / `parse_initial` (including single-child inference and compound-only
  initial); guard strings share the parser's semantics (`!` negation, `stateIn` left to the
  parser — where `compile_guard` requires a state id and errors identically); string actions
  are user actions in both paths, and the direct path declines built-in spellings rather
  than re-classifying them (`builtin::canonical` is shared); `after` keys go through the same
  `Delay::parse_after_key`; event descriptors are normalised identically
  (`names::normalize` in both `compile.rs` and `direct.rs`); `version`/`context`/policies
  handling matches (policies/strictConfig fall back). Anything the direct path cannot accept
  falls back, and `machine/direct_tests` additionally asserts *identical error strings*.
  The 34 new adversarial documents all pass, including the deliberate validations
  ("unguarded `always` self-transition can never make progress",
  "`onDone` is only valid on compound or parallel states") which are errors in both paths.
- **Bitset.** `clear_range` masks are capped and span-correct; `ones_in` and `set` use
  checked indexing where an out-of-capacity index can arise; no OOB reachable from the
  engine's index ranges.
- **Logic registry.** The scan (≤16) and hash (>16) paths are byte-equality-equivalent
  (`text::name_eq` compares UTF-8 bytes, which is string equality); insertion/replacement
  order and the threshold crossing are consistent.
- **Caches and clocks.** `plans`, `initial_entry`, `declared_events` and `hash` are
  `OnceLock`s computed once and immutable; `from_snapshot` builds its configuration from the
  snapshot, never from `initial_entry`. `SyncInterpreter::new` borrowing the process-wide
  `&'static RealClock` is observably identical to holding an `Arc` (one shared origin);
  `with_clock` still takes the caller's `Arc<dyn Clock>`.
- **Lazy `CancelTimers`.** `exit_states` emits `CancelTimers` only for states with `after`,
  and `from_snapshot` refuses deadlines on states without `after` (`interpreter/restore.rs`),
  so the invariant is enforced at both ends; the four `RestartTimers` modes clamp and park
  consistently.

## Limits / not covered here

- Performance numbers (S1–S7, allocations, the 100× claims) belong to §6.3, not this section.
- The 17-machine matrix is a hand-picked sample plus a random differential; it is evidence,
  not a proof. The missing general-path conformance run is exactly finding M4.
- Snapshot layout, structure-hash byte parity and Python interop are covered by the
  conformance/golden suites (§6.0 commit review and §6.2), not re-derived here.

## Handover to the report owner

1. Adopt the depth cap (HIGH-3) and the `Source::to_value` reparse fix (M1) — both are
   local, testable, and behaviour-preserving for every input that works today.
2. Implement the M4 feature design and add the `--no-default-features` conformance leg so the
   §6.0 cross-check can actually run.
3. Promote the scratch tests (L3) or leave them in `docs/qa/work/` as evidence.
