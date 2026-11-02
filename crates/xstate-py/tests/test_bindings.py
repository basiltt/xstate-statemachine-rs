"""Tests for the ``xstate_rs`` bindings.

Run after ``maturin develop`` in a virtualenv that also has the pure-Python
``xstate-statemachine`` installed (it is the byte-equality oracle)::

    python -m pytest crates/xstate-py/tests -q
"""

from __future__ import annotations

import gc
import json
import re
import sys
import weakref
from pathlib import Path
from typing import Any, Dict, List

import pytest

import xstate_rs as x

CASES = Path(__file__).resolve().parents[3] / "conformance" / "cases"

TOGGLE: Dict[str, Any] = {
    "id": "toggle",
    "initial": "inactive",
    "states": {
        "inactive": {"on": {"TOGGLE": "active"}},
        "active": {"on": {"TOGGLE": "inactive"}},
    },
}


def counter_machine(policy: str = "fail") -> x.Machine:
    return x.Machine.from_dict(
        {
            "id": "counter",
            "initial": "idle",
            "actionErrorPolicy": policy,
            "context": {"count": 0, "log": []},
            "states": {
                "idle": {
                    "on": {
                        "INC": {
                            "guard": {"type": "belowMax", "params": {"max": 3}},
                            "actions": ["bump", "remember"],
                        },
                        "BOOM": {"target": "broken", "actions": ["explode"]},
                        "GO": "busy",
                    }
                },
                "busy": {"on": {"BACK": "idle"}},
                "broken": {},
            },
        }
    )


def counter_logic(**extra_actions: Any) -> x.MachineLogic:
    def bump(ctx: Dict[str, Any], event: Dict[str, Any], params: Any) -> None:
        ctx["count"] += event.get("by", 1)

    def remember(ctx: Dict[str, Any], event: Dict[str, Any], params: Any) -> Dict[str, Any]:
        return {"log": ctx["log"] + [event["type"]]}

    def explode(ctx: Dict[str, Any], event: Dict[str, Any], params: Any) -> None:
        raise ValueError("kaboom")

    def below_max(ctx: Dict[str, Any], event: Dict[str, Any], params: Any) -> bool:
        return ctx["count"] < params["max"]

    actions = {"bump": bump, "remember": remember, "explode": explode}
    actions.update(extra_actions)
    return x.MachineLogic(actions=actions, guards={"belowMax": below_max})


# --------------------------------------------------------------------------- machine


def test_machine_loads_from_json_and_dict() -> None:
    from_dict = x.Machine.from_dict(TOGGLE)
    from_json = x.Machine.from_json(json.dumps({**TOGGLE, "version": "2"}))
    assert from_dict.id == "toggle"
    assert from_dict.version is None
    assert from_json.version == "2"
    assert from_dict.state_ids == ["toggle", "toggle.inactive", "toggle.active"]
    assert from_dict.declared_events == ["TOGGLE"]
    assert len(from_dict.structure_hash) == 16
    assert from_dict.structure_hash == x.structure_hash(from_dict)
    assert json.loads(from_dict.to_json()) == TOGGLE
    assert from_dict.to_dict() == TOGGLE
    assert "stateDiagram-v2" in from_dict.to_mermaid()
    assert "@startuml" in from_dict.to_plantuml()
    assert "toggle" in repr(from_dict)


def test_invalid_configs_raise_config_errors() -> None:
    with pytest.raises(x.ConfigError) as bad_json:
        x.Machine.from_json("{not json")
    assert bad_json.value.kind == "invalid_config"
    bad_target = {"id": "m", "initial": "a", "states": {"a": {"on": {"GO": "nowhere"}}}}
    with pytest.raises(x.StateNotFoundError) as missing:
        x.Machine.from_dict(bad_target)
    assert isinstance(missing.value, x.ConfigError)
    assert isinstance(missing.value, x.XStateError)
    with pytest.raises(x.ConfigError):
        x.Machine.from_dict({"id": "m", "initial": "zzz", "states": {"a": {}}})
    with pytest.raises(TypeError):
        x.Machine.from_dict({"id": "m", "context": {"bad": object()}, "states": {}})


def test_validate_checks_logic_names() -> None:
    machine = counter_machine()
    assert machine.validate() == []
    assert machine.validate(counter_logic()) == []
    with pytest.raises(x.ImplementationMissingError) as missing:
        machine.validate(x.MachineLogic(actions={"bump": lambda c, e, p: None}))
    assert missing.value.kind == "implementation_missing"
    interp = x.SyncInterpreter(machine)
    with pytest.raises(x.ImplementationMissingError):
        interp.start()


def test_machine_logic_rejects_bad_registrations() -> None:
    with pytest.raises(TypeError):
        x.MachineLogic(actions={"a": 42})
    with pytest.raises(TypeError):
        x.MachineLogic(delays={"d": "soon"})
    with pytest.raises(ValueError):
        x.MachineLogic(delays={"d": -5})
    logic = x.MachineLogic(delays={"d": 5, "f": lambda c, e: 1})
    assert logic.delays["d"] == 5.0
    assert callable(logic.delays["f"])


# --------------------------------------------------------------------------- interpreter


def test_toggle_with_actions_and_guards_mutates_context() -> None:
    interp = x.SyncInterpreter(counter_machine(), counter_logic())
    assert interp.status == "uninitialized"
    assert interp.context is None
    start = interp.start()
    assert start.state_ids == frozenset({"counter.idle"})
    assert start.changed and start.status == "running"

    receipt = interp.send("INC", by=2)
    assert receipt.actions == ["bump", "remember"]
    assert receipt.changed and not receipt.unhandled and receipt.error is None
    assert interp.context == {"count": 2, "log": ["INC"]}

    receipt = interp.send({"type": "INC"})
    assert interp.context == {"count": 3, "log": ["INC", "INC"]}
    # The guard (count < params.max) now refuses.
    refused = interp.send("INC")
    assert refused.actions == [] and not refused.changed
    assert interp.context["count"] == 3

    # `context` is a copy: mutating it does not reach the machine.
    interp.context["count"] = 99
    assert interp.context["count"] == 3

    assert interp.send("GO").state_ids == {"counter.busy"}
    assert interp.matches("counter.busy") and interp.matches("busy")
    assert interp.value == "busy"
    assert interp.state_ids == ["counter.busy"]
    assert interp.current_state_ids == {"counter.busy"}
    assert interp.send("NOPE").unhandled
    interp.stop()
    assert interp.status == "stopped"


def test_send_accepts_event_objects_and_rejects_garbage() -> None:
    from xstate_statemachine import Event

    seen: List[Dict[str, Any]] = []
    machine = x.Machine.from_dict(
        {"id": "m", "initial": "a", "states": {"a": {"on": {"PING": {"actions": "spy"}}}}}
    )
    logic = x.MachineLogic(actions={"spy": lambda c, e, p: seen.append(e)})
    interp = x.SyncInterpreter(machine, logic)
    interp.start()
    interp.send(Event(type="PING", payload={"n": 1}), extra=True)
    assert seen == [{"type": "PING", "n": 1, "extra": True}]
    # The Python library's `wait=` / `priority=` flags are accepted and never become payload.
    assert interp.send("PING", wait=True, priority=False).actions == ["spy"]
    assert seen[-1] == {"type": "PING"}
    with pytest.raises(x.InvalidEventError):
        interp.send(42)
    with pytest.raises(x.InvalidEventError):
        interp.send({"no": "type"})
    with pytest.raises(x.InvalidEventError):
        interp.send("")


def test_delayed_transitions_with_simulated_clock() -> None:
    machine = x.Machine.from_dict(
        {
            "id": "retry",
            "initial": "waiting",
            "states": {
                "waiting": {"after": {"BACKOFF": "retrying"}},
                "retrying": {"after": {"500": "gave_up"}},
                "gave_up": {"type": "final"},
            },
        }
    )
    clock = x.SimulatedClock()
    calls: List[Dict[str, Any]] = []

    def backoff(ctx: Dict[str, Any], event: Dict[str, Any]) -> int:
        calls.append(event)
        return 1000

    interp = x.SyncInterpreter(machine, x.MachineLogic(delays={"BACKOFF": backoff}), clock=clock)
    interp.start()
    assert interp.pending_timers == 1
    clock.increment(999)
    assert interp.state_ids == ["retry.waiting"]
    clock.increment(1)
    assert interp.state_ids == ["retry.retrying"]
    assert clock.now_ms == 1000.0 and clock.now() == 1.0
    receipt = interp.advance(500)
    assert receipt.state_ids == {"retry.gave_up"}
    assert interp.status == "done"
    assert calls and calls[0]["type"] == "xstate.init"
    with pytest.raises(ValueError):
        clock.increment(-1)

    no_clock = x.SyncInterpreter(machine, x.MachineLogic(delays={"BACKOFF": 1}))
    no_clock.start()
    with pytest.raises(x.InvalidLifecycleError):
        no_clock.advance(10)


def service_machine() -> x.Machine:
    return x.Machine.from_dict(
        {
            "id": "profile",
            "initial": "idle",
            "context": {"user": None, "problem": None},
            "states": {
                "idle": {"on": {"LOAD": "loading"}},
                "loading": {
                    "invoke": {
                        "src": "fetchUser",
                        "id": "fetchUser",
                        "input": {"id": 7},
                        "onDone": {"target": "ready", "actions": "saveUser"},
                        "onError": {"target": "failed", "actions": "saveProblem"},
                    }
                },
                "ready": {"type": "final"},
                "failed": {},
            },
        }
    )


def service_logic(fetch: Any) -> x.MachineLogic:
    return x.MachineLogic(
        actions={
            "saveUser": lambda c, e, p: {"user": e["output"]},
            "saveProblem": lambda c, e, p: {"problem": e["error"]},
        },
        services={"fetchUser": fetch},
    )


def test_service_returning_a_value_completes_the_invocation() -> None:
    inputs: List[Any] = []

    def fetch(ctx: Dict[str, Any], input: Any) -> Dict[str, Any]:
        inputs.append(input)
        return {"name": "Ada", "id": input["id"]}

    interp = x.SyncInterpreter(service_machine(), service_logic(fetch))
    interp.start()
    receipt = interp.send("LOAD")
    assert receipt.state_ids == {"profile.ready"}
    assert receipt.actions == ["saveUser"]
    assert interp.context["user"] == {"name": "Ada", "id": 7}
    assert inputs == [{"id": 7}]
    assert interp.status == "done"


def test_service_raising_takes_on_error() -> None:
    def fetch(ctx: Dict[str, Any], input: Any) -> Any:
        raise ConnectionError("upstream down")

    interp = x.SyncInterpreter(service_machine(), service_logic(fetch))
    interp.start()
    receipt = interp.send("LOAD")
    assert receipt.state_ids == {"profile.failed"}
    assert receipt.error is None  # a modelled failure, handled by onError
    assert interp.context["problem"] == "upstream down"
    assert interp.status == "running"


# --------------------------------------------------------------------------- errors


def test_exception_hierarchy_and_kinds() -> None:
    for cls in (
        x.ConfigError,
        x.UnknownEventError,
        x.LogicError,
        x.SnapshotError,
        x.InvalidLifecycleError,
        x.InvalidEventError,
        x.LimitExceededError,
    ):
        assert issubclass(cls, x.XStateError)
    assert issubclass(x.XStateError, Exception)
    assert issubclass(x.TransitionFailedError, x.LogicError)
    assert issubclass(x.RunawayChainError, x.LogicError)
    assert issubclass(x.ImplementationMissingError, x.ConfigError)

    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    with pytest.raises(x.InvalidLifecycleError) as early:
        interp.send("TOGGLE")
    assert early.value.kind == "invalid_lifecycle"
    interp.start()
    with pytest.raises(x.InvalidLifecycleError):
        interp.start()

    strict = x.SyncInterpreter(x.Machine.from_dict({**TOGGLE, "strict": True}))
    strict.start()
    with pytest.raises(x.UnknownEventError) as unknown:
        strict.send("BANANA")
    assert unknown.value.kind == "unknown_event"

    with pytest.raises(x.SnapshotError):
        x.SyncInterpreter.from_snapshot(x.Machine.from_dict(TOGGLE), None, "{garbage")

    loop = x.Machine.from_dict(
        {
            "id": "loop",
            "initial": "a",
            "maxIterations": 10,
            "states": {"a": {"always": "b"}, "b": {"always": "a"}},
        }
    )
    with pytest.raises(x.RunawayChainError):
        x.SyncInterpreter(loop).start()


def test_action_exception_under_fail_policy_raises_and_errors_the_machine() -> None:
    interp = x.SyncInterpreter(counter_machine("fail"), counter_logic())
    interp.start()
    with pytest.raises(x.TransitionFailedError) as failed:
        interp.send("BOOM")
    assert failed.value.kind == "transition_failed"
    assert "kaboom" in str(failed.value)
    assert isinstance(failed.value.__cause__, ValueError)
    assert interp.status == "error"
    assert "kaboom" in (interp.error or "")


def test_action_exception_under_rollback_policy_restores_state() -> None:
    interp = x.SyncInterpreter(counter_machine("rollback"), counter_logic())
    interp.start()
    receipt = interp.send("BOOM")
    assert isinstance(receipt.error, ValueError)
    assert str(receipt.error) == "kaboom"
    assert receipt.state_ids == {"counter.idle"}
    assert interp.status == "running"
    # The next event runs cleanly.
    assert interp.send("INC").error is None


def test_action_exception_under_continue_policy_keeps_going() -> None:
    interp = x.SyncInterpreter(counter_machine("continue"), counter_logic())
    interp.start()
    receipt = interp.send("BOOM")
    assert isinstance(receipt.error, ValueError)
    assert receipt.state_ids == {"counter.broken"}
    assert interp.status == "running"


def test_guard_exception_follows_guard_error_policy() -> None:
    def broken_guard(ctx: Any, event: Any, params: Any) -> bool:
        raise KeyError("missing")

    config = {
        "id": "g",
        "initial": "a",
        "states": {"a": {"on": {"GO": {"target": "b", "guard": "check"}}}, "b": {}},
    }
    logic = x.MachineLogic(guards={"check": broken_guard})
    as_false = x.SyncInterpreter(x.Machine.from_dict(config), logic)
    as_false.start()
    receipt = as_false.send("GO")
    assert receipt.state_ids == {"g.a"}
    assert isinstance(receipt.error, KeyError)

    as_true = x.SyncInterpreter(x.Machine.from_dict({**config, "guardErrorPolicy": True}), logic)
    as_true.start()
    assert as_true.send("GO").state_ids == {"g.b"}

    raising = x.SyncInterpreter(x.Machine.from_dict({**config, "guardErrorPolicy": "raise"}), logic)
    raising.start()
    with pytest.raises(x.LogicError) as err:
        raising.send("GO")
    assert isinstance(err.value.__cause__, KeyError)


def test_keyboard_interrupt_in_a_callback_is_never_swallowed() -> None:
    def interrupt(ctx: Any, event: Any, params: Any) -> None:
        raise KeyboardInterrupt

    interp = x.SyncInterpreter(counter_machine("continue"), counter_logic(explode=interrupt))
    interp.start()
    with pytest.raises(KeyboardInterrupt):
        interp.send("BOOM")


def test_reentrant_calls_from_a_callback_are_refused() -> None:
    holder: Dict[str, Any] = {}

    def reenter(ctx: Any, event: Any, params: Any) -> None:
        holder["interp"].send("GO")

    interp = x.SyncInterpreter(counter_machine("fail"), counter_logic(explode=reenter))
    holder["interp"] = interp
    interp.start()
    with pytest.raises(x.TransitionFailedError) as failed:
        interp.send("BOOM")
    assert isinstance(failed.value.__cause__, x.InvalidLifecycleError)


def test_non_json_context_values_fail_the_action() -> None:
    def stash(ctx: Dict[str, Any], event: Any, params: Any) -> None:
        ctx["handle"] = object()

    interp = x.SyncInterpreter(counter_machine("rollback"), counter_logic(explode=stash))
    interp.start()
    receipt = interp.send("BOOM")
    assert isinstance(receipt.error, TypeError)
    assert "handle" not in interp.context


# --------------------------------------------------------------------------- observer


def test_observer_sees_start_and_every_event() -> None:
    seen: List[Any] = []
    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    interp.observe(lambda event, ids, changed: seen.append((event["type"], ids, changed)))
    interp.start()
    interp.send("TOGGLE")
    interp.send("BANANA", size=3)
    assert seen == [
        ("xstate.init", ["toggle.inactive"], True),
        ("TOGGLE", ["toggle.active"], True),
        ("BANANA", ["toggle.active"], False),
    ]
    with pytest.raises(TypeError):
        interp.observe(42)


def test_observer_exceptions_are_reported_not_raised(monkeypatch: pytest.MonkeyPatch) -> None:
    reported: List[Any] = []
    monkeypatch.setattr(sys, "unraisablehook", lambda info: reported.append(info.exc_value))

    def bad_observer(event: Any, ids: Any, changed: Any) -> None:
        raise RuntimeError("observer bug")

    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    interp.observe(bad_observer)
    interp.start()
    assert interp.send("TOGGLE").state_ids == {"toggle.active"}
    assert len(reported) == 2 and all(isinstance(e, RuntimeError) for e in reported)


# --------------------------------------------------------------------------- snapshots


def test_snapshot_round_trip_and_restore() -> None:
    machine, logic = counter_machine(), counter_logic()
    interp = x.SyncInterpreter(machine, logic)
    interp.start()
    interp.send("INC")
    interp.send("GO")
    text = interp.snapshot()
    assert _normalise(text) == _normalise(interp.get_snapshot())
    data = json.loads(text)
    assert data["version"] == x.SNAPSHOT_VERSION == 4
    assert data["machine_hash"] == machine.structure_hash
    assert data["context"] == {"count": 1, "log": ["INC"]}

    restored = x.SyncInterpreter.from_snapshot(machine, logic, text)
    assert restored.state_ids == ["counter.busy"]
    assert restored.context == interp.context
    assert restored.status == "running"
    restored.start()  # resumes; does not re-enter the initial state
    assert restored.send("BACK").state_ids == {"counter.idle"}
    assert restored.send("INC").actions == ["bump", "remember"]

    drifted = x.Machine.from_dict(
        {**json.loads(machine.to_json()), "states": {**json.loads(machine.to_json())["states"], "extra": {}}}
    )
    with pytest.raises(x.SnapshotError) as drift:
        x.SyncInterpreter.from_snapshot(drifted, logic, text)
    assert drift.value.kind == "snapshot"
    assert x.SyncInterpreter.from_snapshot(drifted, logic, text, verify_hash=False).matches("busy")


def test_restore_timer_policies() -> None:
    machine = x.Machine.from_dict(
        {"id": "t", "initial": "a", "states": {"a": {"after": {"1000": "b"}}, "b": {}}}
    )
    clock = x.SimulatedClock()
    interp = x.SyncInterpreter(machine, clock=clock)
    interp.start()
    clock.increment(400)
    text = interp.snapshot()
    assert json.loads(text)["deadlines"][0]["delay_ms"] == 1000

    parked = x.SyncInterpreter.from_snapshot(machine, None, text, restart_timers="parked")
    assert parked.pending_timers == 0

    resume_clock = x.SimulatedClock()
    resume_clock.set(400)
    resumed = x.SyncInterpreter.from_snapshot(machine, None, text, clock=resume_clock)
    resumed.start()
    resume_clock.increment(599)
    assert resumed.state_ids == ["t.a"]
    resume_clock.increment(1)
    assert resumed.state_ids == ["t.b"]

    restart_clock = x.SimulatedClock()
    restarted = x.SyncInterpreter.from_snapshot(
        machine, None, text, restart_timers=True, clock=restart_clock
    )
    restarted.start()
    restart_clock.increment(999)
    assert restarted.state_ids == ["t.a"]

    due_clock = x.SimulatedClock()
    due_clock.set(5000)
    fired = x.SyncInterpreter.from_snapshot(
        machine, None, text, restart_timers="fire_due", clock=due_clock
    )
    assert fired.state_ids == ["t.b"]
    with pytest.raises(ValueError):
        x.SyncInterpreter.from_snapshot(machine, None, text, restart_timers="sometimes")


def test_clock_holds_interpreters_weakly() -> None:
    clock = x.SimulatedClock()
    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE), clock=clock)
    interp.start()
    ref = weakref.ref(interp)
    del interp
    gc.collect()
    assert ref() is None
    clock.increment(10)  # dead interpreters are pruned silently


def _observer_cycle() -> "weakref.ReferenceType[Any]":
    """An interpreter reachable from its own observer: interp -> observer -> cell -> interp.

    (The callback only touches ``holder``: calling back into a busy interpreter is an error.)
    """
    holder: List[Any] = []
    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    interp.observe(lambda event, state_ids, changed: len(holder))
    holder.append(interp)
    interp.start()
    interp.send("TOGGLE")  # the observer runs
    return weakref.ref(interp)


def _logic_cycle() -> "weakref.ReferenceType[Any]":
    """The same through a registered action and a guard."""
    holder: List[Any] = []
    logic = x.MachineLogic(
        actions={"touch": lambda ctx, event, params: None if holder else None},
        guards={"always": lambda ctx, event, params: len(holder) == 1},
    )
    machine = x.Machine.from_dict(
        {
            "id": "g",
            "initial": "a",
            "states": {
                "a": {"on": {"GO": {"target": "b", "guard": "always", "actions": "touch"}}},
                "b": {},
            },
        }
    )
    interp = x.SyncInterpreter(machine, logic)
    holder.append(interp)
    interp.start()
    interp.send("GO")
    assert interp.state_ids == ["g.b"]
    return weakref.ref(interp)


@pytest.mark.parametrize("make_cycle", [_observer_cycle, _logic_cycle])
def test_interpreter_cycles_through_callbacks_are_collected(make_cycle: Any) -> None:
    gc.collect()
    ref = make_cycle()
    gc.collect()
    assert ref() is None, "the cycle through a Python callback was never collected"


def test_clear_leaves_a_working_interpreter_until_collected() -> None:
    # A live interpreter keeps its callbacks: collection must not touch reachable objects.
    seen: List[Any] = []
    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    interp.observe(lambda event, state_ids, changed: seen.append(state_ids))
    interp.start()
    gc.collect()
    interp.send("TOGGLE")
    assert seen[-1] == ["toggle.active"]


# --------------------------------------------------------------------------- Python parity


def _taken_at(text: str) -> str:
    match = re.search(r'"taken_at": [^,\n]+', text)
    assert match, text
    return match.group(0)


def _normalise(text: str) -> str:
    return text.replace(_taken_at(text), '"taken_at": 0.0')


def _load_case(name: str) -> Dict[str, Any]:
    return json.loads((CASES / name).read_text(encoding="utf-8"))


def _run_python(case: Dict[str, Any]) -> Any:
    import logging

    from xstate_statemachine import SimulatedClock, SyncInterpreter, create_machine
    from xstate_statemachine.testing_utils import stub_logic

    logging.getLogger("xstate_statemachine").setLevel(logging.CRITICAL)
    cfg = json.loads(json.dumps(case["machine"]))
    logic = stub_logic(cfg, guards=dict(case.get("logic", {}).get("guards", {})))
    machine = create_machine(cfg, logic=logic)
    interp = SyncInterpreter(machine, clock=SimulatedClock())
    interp.start()
    for step in case["steps"]:
        event = step["send"]
        if isinstance(event, str):
            interp.send(event)
        else:
            interp.send(event["type"], **{k: v for k, v in event.items() if k != "type"})
    return machine, interp, logic


def _run_rust(case: Dict[str, Any], py_logic: Any) -> Any:
    guards = case.get("logic", {}).get("guards", {})
    logic = x.MachineLogic(
        actions={name: (lambda c, e, p: None) for name in py_logic.actions},
        guards={name: (lambda v: lambda c, e, p: v)(guards.get(name, True)) for name in py_logic.guards},
    )
    machine = x.Machine.from_dict(case["machine"])
    interp = x.SyncInterpreter(machine, logic, clock=x.SimulatedClock())
    interp.start()
    for step in case["steps"]:
        interp.send(step["send"])
    return machine, interp


PARITY_CASES = [
    "002_guard_and_assign.json",
    "006_history_shallow.json",
    "016_final_output_done.json",
    "026_ancestor_handler_and_payload.json",
]


@pytest.mark.parametrize("case_file", PARITY_CASES)
def test_snapshot_bytes_and_hash_match_the_python_library(case_file: str) -> None:
    from xstate_statemachine.persistence.snapshot import structure_hash as py_structure_hash

    case = _load_case(case_file)
    py_machine, py_interp, py_logic = _run_python(case)
    rs_machine, rs_interp = _run_rust(case, py_logic)

    assert rs_machine.structure_hash == py_structure_hash(py_machine)
    assert x.structure_hash(rs_machine) == case["expected"]["python"]["machine_hash"]
    py_text = py_interp.get_snapshot()
    rs_text = rs_interp.snapshot()
    assert _normalise(rs_text).encode() == _normalise(py_text).encode()


def test_python_snapshot_restores_in_rust_and_back() -> None:
    from xstate_statemachine import SyncInterpreter as PySyncInterpreter

    case = _load_case("006_history_shallow.json")
    py_machine, py_interp, _ = _run_python(case)
    rs_machine = x.Machine.from_dict(case["machine"])

    rs_interp = x.SyncInterpreter.from_snapshot(rs_machine, None, py_interp.get_snapshot())
    assert rs_interp.current_state_ids == set(py_interp.current_state_ids)
    rs_interp.start()
    rs_interp.send("ASK")

    back = PySyncInterpreter.from_snapshot(rs_interp.snapshot(), py_machine)
    back.start()
    assert set(back.current_state_ids) == {"wizard.help"}
    back.send("BACK")
    rs_interp.send("BACK")
    assert set(back.current_state_ids) == rs_interp.current_state_ids


def test_tricky_context_values_serialise_like_python() -> None:
    from xstate_statemachine import SimulatedClock, SyncInterpreter, create_machine

    context = {
        "floats": [0.0, -0.0, 0.1, 1.5, 1e-4, 1e-5, 1e15, 1e16, 1.5e300, 123456.789, 2.5e-308],
        "ints": [0, -1, 2**53 + 1, -(2**63), 2**64 - 1],
        "text": "café ☃ \U0001f600 \"quoted\" back\\slash\ttab\x7f\x01",
        "nested": {"empty_list": [], "empty_dict": {}, "none": None, "flag": True},
    }
    config = {"id": "values", "initial": "a", "context": context, "states": {"a": {}}}
    py = SyncInterpreter(create_machine(json.loads(json.dumps(config))), clock=SimulatedClock())
    py.start()
    rs = x.SyncInterpreter(x.Machine.from_dict(config))
    rs.start()
    assert rs.context == context
    assert _normalise(rs.snapshot()) == _normalise(py.get_snapshot())


def test_conversion_limits() -> None:
    interp = x.SyncInterpreter(x.Machine.from_dict(TOGGLE))
    interp.start()
    loop: List[Any] = []
    loop.append(loop)
    with pytest.raises(ValueError):
        interp.send("TOGGLE", data=loop)
    with pytest.raises(ValueError):
        interp.send("TOGGLE", big=2**64)
    with pytest.raises(ValueError):
        interp.send("TOGGLE", nan=float("nan"))
    assert interp.send("TOGGLE", tup=(1, 2), keyed={1: "one"}).state_ids == {"toggle.active"}
