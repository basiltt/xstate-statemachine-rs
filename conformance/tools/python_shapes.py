"""Debug helper: print the pre-hash node shapes the Python library computes for a case."""
import json
import sys

from xstate_statemachine import create_machine
from xstate_statemachine.persistence.snapshot import _node_shape
from xstate_statemachine.testing_utils import stub_logic
from xstate_statemachine.validation import walk

for path in sys.argv[1:]:
    case = json.load(open(path, encoding="utf-8"))
    cfg = case["machine"]
    try:
        m = create_machine(cfg, logic=stub_logic(cfg))
    except Exception as exc:  # noqa: BLE001
        print(f"## {case['name']}: {type(exc).__name__}: {exc}")
        continue
    print(f"## {case['name']}")
    for n in walk(m):
        print(json.dumps(_node_shape(n), sort_keys=True))
