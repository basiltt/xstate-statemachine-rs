#!/usr/bin/env python
"""Regenerate the `xstate-eda` golden files from the Python library.

Writes into ``crates/xstate-eda/tests/golden``:

* ``asyncapi.json`` -- ``asyncapi_document`` / ``consumed_events`` /
  ``publish_specs`` for the machines in ``MACHINES`` (plus the options used);
* ``envelopes.json`` -- ``Envelope.to_dict`` / ``to_json`` of fixed envelopes,
  ``from_dict`` outcomes (decoded dict or the exact error message) and
  ``default_event_name`` samples;
* ``dead_letters.json`` -- a ``DeadLetter`` record's ``to_dict`` / ``to_json``
  and the records ``InboundDispatcher`` writes for an unknown type and a
  corrupt envelope (``taken_at`` zeroed);
* ``outbox_py.sqlite`` / ``dlq_py.sqlite`` -- databases written by
  ``SQLiteOutboxStore`` / ``SQLiteDeadLetterStore`` that ``tests/golden.rs``
  reads back with the Rust stores.

Usage: ``python -I conformance/tools/eda_golden.py`` with the Python library
importable (its virtualenv).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

GOLDEN = (
    Path(__file__).resolve().parents[2]
    / "crates"
    / "xstate-eda"
    / "tests"
    / "golden"
)

ORDER = {
    "id": "order",
    "version": "1.2.0",
    "initial": "pending",
    "context": {"total": 0, "customer": "c-1", "card_token": "tok_live"},
    "states": {
        "pending": {
            "on": {
                "PAY": {
                    "target": "paid",
                    "meta": {
                        "publish": {
                            "type": "order.paid",
                            "data": ["total", "customer"],
                        }
                    },
                },
                "CANCEL": {
                    "target": "cancelled",
                    "meta": {"publish": "order.cancelled"},
                },
                "NOTE": {"meta": {"publish": True}},
                "GO NOW": "paid",
                "GO_NOW": "paid",
            }
        },
        "paid": {
            "tags": ["publish"],
            "on": {
                "SHIP": {"target": "shipped", "meta": {"publish": True}},
                "REFUND": "cancelled",
            },
        },
        "shipped": {"type": "final"},
        "cancelled": {"type": "final", "tags": ["publish"]},
    },
}

WORKFLOW = {
    "id": "workflow",
    "initial": "idle",
    "states": {
        "idle": {
            "on": {
                "START": "running",
                "*": "idle",
                "admin.*": "idle",
                "done.invoke.ext": "idle",
                "xstate.after.1.x": "idle",
                "": "running",
            }
        },
        "running": {
            "initial": "fetch",
            "onDone": {
                "target": "finished",
                "meta": {"publish": "workflow.completed"},
            },
            "states": {
                "fetch": {
                    "invoke": {
                        "id": "loader",
                        "src": "load",
                        "onDone": {
                            "target": "store",
                            "meta": {"publish": {"type": "workflow.loaded"}},
                        },
                        "onError": {
                            "target": "#workflow.failed",
                            "meta": {"publish": "workflow.failed"},
                        },
                    },
                    "after": {
                        "5000": {
                            "target": "#workflow.failed",
                            "meta": {"publish": "workflow.failed"},
                        }
                    },
                    "on": {"RETRY": "fetch"},
                },
                "store": {
                    "tags": ["publish", "busy"],
                    "always": {"target": "saved", "meta": {"publish": True}},
                },
                "saved": {"type": "final"},
            },
        },
        "finished": {"type": "final"},
        "failed": {"id": "failed", "on": {"RESET": "idle"}},
    },
}

SILENT = {
    "id": "silent",
    "initial": "a",
    "states": {"a": {"tags": ["publish"]}},
}

MACHINES = [
    {"name": "order", "config": ORDER, "options": {}},
    {
        "name": "order_with_server",
        "config": ORDER,
        "options": {
            "server": {"host": "localhost:9092", "protocol": "kafka"},
            "inbound_topic": "orders.commands",
            "outbound_topic": "orders.events",
            "title": "Orders",
        },
    },
    {"name": "workflow", "config": WORKFLOW, "options": {}},
    {"name": "silent", "config": SILENT, "options": {}},
]

ENVELOPE_ID = "0190a0b4-1c2d-7e3f-8a4b-5c6d7e8f9a0b"
ENVELOPE_TIME = "2024-07-01T12:00:00.123456Z"
TRACEPARENT = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"


def asyncapi_cases() -> list:
    from xstate_statemachine import MachineLogic, create_machine
    from xstate_statemachine.eda import (
        asyncapi_document,
        consumed_events,
        publish_specs,
    )

    logic = MachineLogic(services={"load": lambda *args: None})
    out = []
    for case in MACHINES:
        machine = create_machine(case["config"], logic=logic)
        out.append(
            {
                "name": case["name"],
                "config": case["config"],
                "options": case["options"],
                "document": asyncapi_document(machine, **case["options"]),
                "consumed": consumed_events(machine),
                "publish_specs": publish_specs(machine),
            }
        )
    return out


def from_dict_case(raw: object) -> dict:
    from xstate_statemachine.eda import Envelope, EnvelopeCorruptError

    try:
        env = Envelope.from_dict(raw)
    except EnvelopeCorruptError as exc:
        return {"input": raw, "error": str(exc)}
    out = env.to_dict()
    if not raw.get("id"):
        out["id"] = "<generated>"
    return {"input": raw, "ok": out}


def envelope_cases() -> dict:
    from xstate_statemachine.eda import Envelope, default_event_name

    base = {
        "specversion": "1.0",
        "id": ENVELOPE_ID,
        "type": "xsm.order.PAY",
        "source": "xsm/order",
    }
    full = Envelope(
        type="xsm.order.PAY",
        source="xsm/order",
        subject="order-42",
        id=ENVELOPE_ID,
        time=ENVELOPE_TIME,
        data={"total": 12.5, "note": "café 日本", "items": [1, None]},
        correlationid="corr-1",
        causationid="cause-1",
        machineid="order",
        machineversion="1.2.0",
        extensions={"traceparent": TRACEPARENT, "xsmattempt": 2, "tenant": "t1"},
    )
    minimal = Envelope(type="t", source="s", id=ENVELOPE_ID)
    deep = {"a": 1}
    for _ in range(70):
        deep = {"n": deep}
    raws = [
        base,
        dict(base, datacontenttype=None, subject="s-1", data=None),
        dict(base, data=[1, 2], tenant="t1", flag=True, count=3),
        dict(base, id=""),
        {"id": ENVELOPE_ID, "type": "t", "source": "s"},
        {"specversion": "1.0", "type": "t", "source": "s"},
        dict(base, specversion="2.0"),
        dict(base, type=""),
        dict(base, type=5),
        dict(base, subject=7),
        dict(base, subject=""),
        dict(base, source="x" * 1025),
        dict(base, Tenant="x"),
        dict(base, authtoken="x"),
        dict(base, apikey="x"),
        dict(base, tenant=1.5),
        dict(base, tenant={"a": 1}),
        dict(base, traceparent="00-abc"),
        dict(base, traceparent=TRACEPARENT),
        dict(base, xsmattempt=-1),
        dict(base, xsmattempt=True),
        dict(base, xsmattempt="2"),
        dict(base, waytoolongextensionname1="x"),
        dict(base, data=deep),
    ]
    return {
        "full": {"dict": full.to_dict(), "json": full.to_json()},
        "minimal": {"dict": minimal.to_dict(), "json": minimal.to_json()},
        "from_dict": [from_dict_case(r) for r in raws],
        "default_event_name": {
            t: default_event_name(t)
            for t in ["xsm.order.PAY", "xsm.order", "xsm.order.", "order.paid",
                      "xsm.a.b.c", "xsm..X"]
        },
    }


def dispatcher_records() -> list:
    from xstate_statemachine import create_machine
    from xstate_statemachine.eda import Envelope, InboundDispatcher
    from xstate_statemachine.eda import MemoryDeadLetterStore
    from xstate_statemachine.persistence import MemoryStore

    store = MemoryStore()
    dlq = MemoryDeadLetterStore()
    machine = create_machine(ORDER)
    disp = InboundDispatcher(store, {"xsm.order.PAY": machine}, dead_letters=dlq)
    unknown = Envelope(
        type="xsm.other.GO",
        source="xsm/other",
        subject="o-1",
        id=ENVELOPE_ID,
        time=ENVELOPE_TIME,
        data={"password": "hunter2", "n": 1},
    )
    corrupt = Envelope(
        type="xsm.order.PAY",
        source="xsm/order",
        subject="o-2",
        id="0190a0b4-1c2d-7e3f-8a4b-5c6d7e8f9a0c",
        data=[1, 2],
    )
    out = []
    for env in (unknown, corrupt):
        res = disp.handle(env, topic="orders")
        rec = dlq.get(env.id).to_dict()
        rec["taken_at"] = 0.0
        out.append({"outcomes": res.outcomes, "record": rec})
    return out


def dead_letter_cases() -> dict:
    from xstate_statemachine.patterns.dead_letter import DeadLetter

    rec = DeadLetter(
        machine_id="order",
        state_id="order.dead",
        event={"type": "PAY", "payload": {"n": 1, "token": "***"}},
        attempts=3,
        errors=[
            {"source": "service", "name": "charge", "type": "RuntimeError",
             "message": "declined é"},
        ],
        snapshot={"context": {"total": 1}},
        taken_at=1700000000.25,
        id="rec-1",
        reason="max_attempts",
        envelope={"id": "rec-1", "type": "xsm.order.PAY"},
        topic="orders",
        machine_hash="abcdef0123456789",
        machine_version="1.2.0",
    )
    return {
        "record": {"dict": rec.to_dict(), "json": rec.to_json()},
        "dispatcher": dispatcher_records(),
    }


def write_sqlite() -> None:
    from xstate_statemachine.eda import (
        Envelope,
        SQLiteDeadLetterStore,
        SQLiteOutboxStore,
    )
    from xstate_statemachine.patterns.dead_letter import DeadLetter

    for name in ("outbox_py.sqlite", "dlq_py.sqlite"):
        for suffix in ("", "-wal", "-shm"):
            p = GOLDEN / (name + suffix)
            if p.exists():
                p.unlink()
    outbox = SQLiteOutboxStore(str(GOLDEN / "outbox_py.sqlite"))
    for n in range(3):
        outbox.add(
            "orders",
            Envelope(
                type="order.paid",
                source="xsm/order",
                subject=f"o-{n}",
                id=f"0190a0b4-1c2d-7e3f-8a4b-5c6d7e8f9a1{n}",
                time=ENVELOPE_TIME,
                data={"n": n, "note": "日本"},
            ),
        )
    outbox.mark_sent([1])
    outbox.claim(limit=1, owner="py-relay", lease_s=100 * 365 * 86400.0)
    outbox.close()
    dlq = SQLiteDeadLetterStore(str(GOLDEN / "dlq_py.sqlite"))
    dlq.put(
        DeadLetter(
            machine_id="order",
            state_id="",
            event={"type": "xsm.order.PAY", "payload": {"password": "p", "n": 1}},
            attempts=5,
            errors=[{"source": "dispatcher", "name": "max_attempts",
                     "type": "ProcessingFailedError", "message": "boom"}],
            snapshot={},
            taken_at=1700000000.5,
            id="env-1",
            reason="max_attempts",
            envelope={"id": "env-1", "type": "xsm.order.PAY",
                      "source": "xsm/order", "specversion": "1.0",
                      "subject": "o-1", "data": {"n": 1}},
            topic="orders",
            machine_version="1.2.0",
        )
    )
    dlq.audit("purge", "env-1", "cleanup", {"n": 1}, actor="ops")
    dlq.close()


def dump(name: str, value: object) -> None:
    path = GOLDEN / name
    path.write_text(
        json.dumps(value, indent=2, ensure_ascii=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    print(f"wrote {path.name}")


def main() -> int:
    GOLDEN.mkdir(parents=True, exist_ok=True)
    dump("asyncapi.json", asyncapi_cases())
    dump("envelopes.json", envelope_cases())
    dump("dead_letters.json", dead_letter_cases())
    write_sqlite()
    print("wrote outbox_py.sqlite, dlq_py.sqlite")
    return 0


if __name__ == "__main__":
    sys.exit(main())
