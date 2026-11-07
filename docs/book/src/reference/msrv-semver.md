# MSRV and SemVer

## Minimum supported Rust version

The workspace declares Rust **1.88** (edition 2024) and every feature builds on it **except
`postgres`** — and therefore `full`, which includes it — because `sqlx` 0.9 requires Rust 1.94. The
CI `MSRV` job checks the workspace and every other feature on 1.88 and skips `postgres`; the other
jobs run on stable, which covers it.

## Versioning

Every crate of the workspace shares one version number. The workspace is at **0.1.0 and is
unpublished** (pre-release). Until 0.1.0 ships, the public API is not a semver guarantee: what is
`pub` today is the surface that will be stabilised or changed before the first release.

Once released:

- The MSRV is raised only in a minor release (per the changelog's policy statement).
- Semantic Versioning governs breaking changes; 0.x minor releases may still change the API.

## Related policy

- `unsafe_code = "forbid"` in every crate; no unsafe anywhere.
- `cargo-deny` (advisories, bans, licenses, sources) and `cargo-audit` run in CI; TLS is rustls
  (`openssl-sys` is banned).
- The design decisions behind these choices are in [ADR-0005](adr.md).
