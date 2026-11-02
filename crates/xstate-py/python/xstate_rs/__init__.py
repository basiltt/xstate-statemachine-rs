"""The ``xstate-statemachine`` synchronous API running on the Rust engine.

``xstate_rs`` mirrors the pure-Python library's ``SyncInterpreter`` surface -- machines,
``MachineLogic`` registries, receipts, snapshots -- while every step runs in ``xstate-core``.
Snapshots use the Python library's layout v4 and formatting and ``structure_hash`` is
identical, so both engines can share one store.

Example::

    import xstate_rs

    machine = xstate_rs.Machine.from_dict({
        "id": "toggle", "initial": "off",
        "states": {"off": {"on": {"FLIP": "on"}}, "on": {"on": {"FLIP": "off"}}},
    })
    interp = xstate_rs.SyncInterpreter(machine)
    interp.start()
    assert interp.send("FLIP").state_ids == {"toggle.on"}
"""

from .xstate_rs import (
    SNAPSHOT_VERSION,
    ConfigError,
    ImplementationMissingError,
    InvalidEventError,
    InvalidLifecycleError,
    LimitExceededError,
    LogicError,
    Machine,
    MachineLogic,
    Receipt,
    RunawayChainError,
    SimulatedClock,
    SnapshotError,
    StateNotFoundError,
    SyncInterpreter,
    TransitionFailedError,
    UnknownEventError,
    XStateError,
    __version__,
    structure_hash,
)

__all__ = [
    "SNAPSHOT_VERSION",
    "ConfigError",
    "ImplementationMissingError",
    "InvalidEventError",
    "InvalidLifecycleError",
    "LimitExceededError",
    "LogicError",
    "Machine",
    "MachineLogic",
    "Receipt",
    "RunawayChainError",
    "SimulatedClock",
    "SnapshotError",
    "StateNotFoundError",
    "SyncInterpreter",
    "TransitionFailedError",
    "UnknownEventError",
    "XStateError",
    "__version__",
    "structure_hash",
]
