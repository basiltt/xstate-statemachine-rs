# Python bindings

`xstate-py` (package `xstate-py`, `publish = false`) wraps the `xstate-core` engine in PyO3,
published as the `xstate_rs` extension module. It is the Python library's own API backed by the
Rust engine rather than the pure-Python one: `create_machine` / `Machine`, `MachineLogic`,
`SyncInterpreter`, receipts and snapshots, with the same layout-v4 snapshots and identical
`structure_hash`, so the two engines share stores.

```python
import xstate_rs

machine = xstate_rs.Machine.from_dict({
    "id": "toggle", "initial": "off",
    "states": {"off": {"on": {"FLIP": "on"}}, "on": {"on": {"FLIP": "off"}}},
})
interp = xstate_rs.SyncInterpreter(machine)
interp.start()
assert interp.send("FLIP").state_ids == {"toggle.on"}
```

Status: **in progress** — the crate is not published to crates.io and its wheel build is not yet in
CI. The module mirrors the synchronous API; `logic` wraps Python callables as engine
implementations, `pyjson` reproduces Python's `json.dumps(indent=2)` for snapshots, and `errors`
maps engine errors onto the exception hierarchy. Treat the surface as unstable until 0.1.0.

The rest of this book covers the Rust API; for parity between the pure-Python library and the Rust
crates, see [migrating from Python](migrating-from-python.md).
