# Security policy

## Supported versions

The project has not had its first public release yet (workspace version 0.1.0 is unpublished).
Once released, only the latest published version of each crate receives security fixes; fixes
are released in patch versions where semver allows.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub's **Report a vulnerability**
button on the repository (`https://github.com/basiltt/xstate-statemachine-rs`, available once the
repository is public; the maintainer's GitHub account is `basiltt`). If you cannot use GitHub,
open a minimal public issue asking for a private channel, without disclosing any details of the
issue. Include:

- the affected crate(s) and version,
- a description of the issue,
- a minimal reproduction (a machine JSON, a code snippet, or a request sequence for the HTTP
  surfaces),
- any information about impact you have.

Please do not open a public issue for a suspected vulnerability before the maintainer has had a
chance to respond. You will receive an acknowledgement within five business days and an update
on the disposition as soon as one exists.

## What to expect

- **Confirmed** issues are fixed on the current release branch, backported where feasible, and
  disclosed with an advisory after a patch release.
- **Not-applicable** reports receive a short explanation.

## Security-relevant surfaces

The following surfaces are security-sensitive and get particular review attention: the machine
JSON parser (`Machine::from_json`, which must never panic on untrusted input), snapshot restore,
the CloudEvents `Envelope` parser, the `SseSink` HTTP/SSE server and `xsm inspect --live`, the
axum HTTP routes (RFC 9457 problems, idempotency keys, authorizers, Origin rules), and the
Python bindings. The workspace forbids `unsafe` code (`unsafe_code = "forbid"`) and runs
cargo-deny (advisories, bans, licenses, sources) and cargo-audit in CI.

## Threat model

The threat model for the parser, snapshot restore, EDA envelopes, the SSE inspector and the
HTTP surfaces ships with this repository at
[`docs/book/src/operations/security.md`](docs/book/src/operations/security.md) and with the
documentation site.
