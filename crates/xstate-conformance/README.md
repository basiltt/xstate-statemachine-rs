# xstate-conformance

Typed model of the conformance fixtures in [`conformance/cases/`](../../conformance) and the
runner that drives the Rust engine through them. A case pairs an XState JSON machine with stubbed
logic, a step script and the behaviour observed by two oracles: the real XState v5 engine
(Node) and the Python library. The Rust engine must match XState's observations and reproduce
Python's snapshot bytes and structure hash.

Status: 0.1.0, **not published** (`publish = false`): it backs this workspace's own test suite.
45 cases; the engine matches XState on all of them. Cases where the two oracles disagree are
listed in [`conformance/divergences.generated.md`](../../conformance/divergences.generated.md).

```bash
cargo test -p xstate-conformance          # fixtures, engine and snapshot vectors
```

Regenerate the expectations (needs Node 20+ and the Python library):

```bash
npm ci --prefix conformance/tools/xstate_oracle
python conformance/tools/generate_expected.py
```

## Features

None.

## Python counterpart

The Python library's own parity tests (`tests/test_xstate_v5_parity.py`,
`test_scxml_correctness.py`, `test_engine_conformance.py`) are the other oracle.
