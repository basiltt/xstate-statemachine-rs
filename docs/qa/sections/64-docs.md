# §6.4 Documentation and the GitHub Pages site ("the bible")

Scope: the §6.4 mandate — the mdBook site under `docs/book/`, the rustdoc `/api/` tree, the
`pages.yml` workflow, and the link check. Prepared before the GitHub remote exists, so the
workflow is written but never run, nothing is deployed, and the site URL
(`https://basiltt.github.io/xstate-statemachine-rs/`) is not live yet.

Severity tally: CRITICAL 0 · HIGH 1 (not this slice, see HIGH-1) · MEDIUM 4 · LOW 5. This slice is
documentation only; no Rust source, tests, other workflows or the Python repo were touched. Edits
were limited to `docs/book/**`, five cross-doc links in `docs/guide/*.md`,
`.github/workflows/pages.yml`, and this file.

## What exists

A 35-page book, restructured from the previous ~60-page `SUMMARY.md` (which listed pages that did
not exist and could not build):

- Introduction, Getting started (getting-started + stately-editor), Concepts (overview + 6),
  Guides (12), Operations (security + deployment + monitoring + backup/restore), Interoperability
  (migrating-from-python), Reference (feature flags + env vars + error catalogue + MSRV/SemVer +
  ADRs + feasibility report), Contributing, Changelog.
- Folded in verbatim with `{{#include}}`: the six `docs/guide/*.md` guides, the five
  `docs/adr/*.md` ADRs, seven per-crate READMEs (tokio, patterns, observability, axum, agents,
  macros, xsm/CLI), `CONTRIBUTING.md` and `CHANGELOG.md`.
- Original short pages where a section needed framing: the concepts pages, sync interpreter,
  inspector, Python bindings, deployment, monitoring, backup/restore, env vars, error catalogue
  pointer, MSRV/SemVer, the ADR framing page and a feasibility-report stub.

The per-crate READMEs with repo-relative links (`xstate-core`, `xstate-inspect`,
`xstate-statemachine`, `xstate-conformance`) were **not** included raw — those links cannot be
rewritten (the files are outside the permitted edit scope), so their crates are covered by the
guides and original pages instead.

## Build and link-check results

Exact commands, run on the existing shared `target/` (mdBook 0.5.4, Rust 1.99.1, Python 3.14.6):

```bash
mdbook build docs/book                                          # exit 0
python -I target/tmp/linkcheck.py docs/book/book                # 255 refs, 0 missing, exit 0
cargo doc --workspace --no-deps --all-features --locked         # exit 101; see HIGH-1 (not this slice)
```

- `book.toml` was fixed to build on mdBook 0.5.4: the unknown `multilingual` key was removed, the
  `additional-css` path was corrected to `src/theme/custom.css`, and the `[output.linkcheck]`
  section was dropped because `mdbook-linkcheck` is not installed (the link check below replaces
  it).
- The link check is a stdlib-only Python script (`html.parser`, no dependencies) at
  `target/tmp/linkcheck.py` (scratch, not committed). It walks the 38 generated HTML files, resolves
  every relative `<a href>` / `<img src>` against the filesystem, and reports missing targets and
  root-relative refs. Result: 255 relative refs checked, **0 missing, 0 root-relative**.
- `cargo doc` writes the crates under `target/doc/` as `xstate_core/`, `xstate_tokio/`, … and the
  Pages workflow copies that tree to `site/api/`, matching the `/api/<crate>/` links the book uses.
  The existing `target/doc/` was inspected to confirm the crate directory names, but a fresh
  `cargo doc --workspace --no-deps --all-features --locked` **does not currently compile** — see
  HIGH-1; the failure is an unrelated in-flight `xstate-eda` edit, not this slice.

## Link fixes made in the source guides

Includes are resolved relative to the *including* book page, not the source file, so the guides'
cross-document links had to be handled. The only `docs/guide/*.md` edits were the five links that
point outside the sibling guide set, now absolute GitHub URLs (blob/main):

- `stately-editor.md`: `../../conformance/divergences.generated.md`
- `persistence.md`: `../../README.md#development` (×2)
- `eda.md`: `../../README.md#development`
- `migrating-from-python.md`: `../../README.md#interoperability-with-the-python-library`

Guide-to-guide links (bare filenames like `features.md`, `persistence.md`) were left alone: the
guides are all siblings under `src/guide/`, so those resolve in the book and still work on GitHub.
The ADRs' `../feasibility-report.md` links resolve against a real `src/feasibility-report.md` stub.

## Findings (documentation gaps, ranked)

### HIGH-1 (not this slice): `cargo doc --all-features` does not compile while `xstate-eda` is mid-edit

**Status: FIXED (`83b0b6e`) and re-verified.** The EDA settle fix landed; at `7ff069d`+,
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked` passes
(the check that the Pages workflow runs).

`crates/xstate-eda/src/rabbitmq/transport.rs` and `crates/xstate-eda/src/async_core.rs` carry
uncommitted edits (another agent's battle-test fix, not this slice) that leave `xstate-eda` unable
to compile under `--all-features`: `transport.rs` `ack`/`drop_message` now return a
`Result<bool, BrokerError>` settlement value while their signatures still declare
`Result<(), BrokerError>`, so `cargo doc --workspace --no-deps --all-features --locked` fails with
`error[E0308]: mismatched types` and `could not compile xstate-eda (lib)`. The `pages.yml`
rustdoc step cannot pass until that edit lands. Out of scope here (Rust source is not to be
touched by this documentation slice); recorded so the Pages build is not assumed green.

### MEDIUM-1: no runnable examples or doctest harness for the book

§6.4 item 2 asks for runnable examples per scenario and `mdbook-test`/doctests so every snippet
compiles in CI. The folded-in guide snippets are compiled by `crates/xstate-statemachine/tests/guide.rs`,
but the book has no `mdbook-test` wiring, and the original pages (concepts, sync interpreter,
inspect, monitoring, deployment, backup/restore) contain code that is illustrative but not compiled
from the book source. A snippet can drift from the API without the book noticing. This is the most
important gap for a "bible".

### MEDIUM-2: guides are single long pages, not the §6.4 per-feature split

§6.4 item 1 lists persistence-per-store/locks/durable-timers/idempotency/log/migration, EDA-per-broker,
a CLI reference with every command/flag/exit code, and the axum full route table as separate pages.
This slice keeps `persistence.md` and `eda.md` as one page each (the existing guide text), folds the
xsm README (commands, not every flag/exit code), and has no full axum route table. The material is
present but not at the granularity the mandate specifies.

### MEDIUM-3: error and metric catalogues are pointers, not complete tables

`reference/errors.md` lists the `ErrorKind` variants and the per-crate error types but points to
rustdoc for the full variant/field list and the HTTP status mapping; `operations/monitoring.md`
lists the `xstatemachine_*` names but the definitive metric catalogue is deferred to rustdoc. §6.4
wants "every error variant and problem type" and a full metric catalogue on the page.

### MEDIUM-4: no Cookbook / examples section

§6.4 lists "Cookbook / examples" as a top-level area. It is out of scope for this slice (page
budget); a cookbook of the traffic-light / order-saga / outbox / crash-resume / durable-timer / HTTP
scenarios would carry most of MEDIUM-1's weight.

### LOW-1: feasibility report is a stub

`feasibility-report.md` is a short pointer to the GitHub file rather than the folded-in text, because
the report is historical and predates the ADRs. Acceptable, but the ADRs' links land on a thin page.

### LOW-2: the ADR page has five H1 headings

The five ADRs are concatenated into one `reference/adr.md` page, each bringing its own `# ADR-000N`
H1. Cosmetic; splitting into five pages would cost five SUMMARY entries.

### LOW-3: Pages workflow is unverified

`pages.yml` (build book + rustdoc → `site/`, upload-pages-artifact, deploy-pages) is prepared but
never run: the remote does not exist and GitHub Pages is not enabled. It mirrors the repo's
`checkout@v4` / `dtolnay/rust-toolchain@stable` / `Swatinem/rust-cache@v2` conventions and installs
mdBook 0.5.4 via `taiki-e/install-action@v2` (the repo already uses `taiki-e/install-action` for
cargo-llvm-cov).

### LOW-4: not verified on mobile, and versioned-docs plan not addressed

mdBook provides search and a responsive layout by default, but rendering was not checked in a
browser; there is no versioned-docs plan (future, once versions ship).

### LOW-5: `custom.css` content not reviewed

The pre-existing `theme/custom.css` is preserved and copied into the output, but its contents were
not reviewed.

## Verified sound

- `mdbook build docs/book` succeeds on mdBook 0.5.4 (exit 0), and every page in `SUMMARY.md`
  exists on disk (no dangling SUMMARY entries).
- The link checker reports **0 missing relative targets and 0 root-relative refs** across all 38
  HTML files, so every internal link resolves; the only book links that leave the site are the
  intentional absolute GitHub URLs (the five fixed guide links, rustdoc, and external references).
- The six guides, five ADRs, seven crate READMEs, CONTRIBUTING and CHANGELOG are folded in and
  render (verified by marker strings in the generated HTML, e.g. all five ADR headings present).
- Feature-flag, MSRV, env-var, policy-default, metric-name, error-kind, `ActionScope` and
  interpreter-API claims on the original pages were checked against the source
  (`crates/xstate-core/src/config/mod.rs`, `logic.rs`, `error.rs`, `interpreter.rs`, `lib.rs`;
  `crates/xstate-observability/src/metric_names.rs`; the README env-var table) before being written.
  Defaults recorded: `actionErrorPolicy` `fail`, `guardErrorPolicy` `false`, `onUnhandled`
  `ignore`, `maxIterations` 1000, `strict*` `false`, `DEFAULT_MAX_INPUT_BYTES` 1 MiB.
- The deployment page's wasm32 claim was verified after this slice:
  `cargo check -p xstate-core --target wasm32-unknown-unknown --no-default-features --locked`
  passes (the target was installed for the check). The fast path is a `cfg` and does not add a
  platform dependency.
- The rustdoc `/api/<crate>/` target names match `target/doc/` (`xstate_core`, `xstate_tokio`,
  `xstate_persistence`, `xstate_eda`, `xstate_patterns`, `xstate_observability`, `xstate_inspect`,
  `xstate_axum`, `xstate_agents`, `xstate_macros`, `xstate_statemachine`, `xstate_testing`,
  `xstate_rs`, `xsm`).
