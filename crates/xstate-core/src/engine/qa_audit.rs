//! Adversarial fast-path qualification matrix: structural cases the proptest generator never
//! produces, each differentially checked fast-vs-general with its plain-transition count
//! asserted. Promoted from the QA audit (`docs/qa/sections/60-perf-paths.md`, finding L3).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::algorithm::FAST_PATH;
use crate::{Machine, MachineLogic, SyncInterpreter, TransitionId};

fn logic() -> MachineLogic<Value> {
    let mut logic = MachineLogic::dynamic()
        .guard("g", |ctx, _, _| Ok(ctx.get("n").and_then(Value::as_u64).unwrap_or(0) < 3))
        .action("act", |_| Ok(()))
        .action("exit_root", |_| Ok(()))
        .action("entry_c", |_| Ok(()))
        .action("exit_d", |_| Ok(()))
        .action("entry_b", |_| Ok(()));
    for name in ["entry_a", "exit_a", "entry_b2", "exit_b2", "entry_e", "exit_e"] {
        logic = logic.action(name, |_| Ok(()));
    }
    logic
}

/// Everything observable after one operation: receipt fields and runtime state.
fn observe(interp: &SyncInterpreter<Value>, receipt: &crate::Result<crate::Receipt>) -> String {
    let receipt = match receipt {
        Ok(r) => format!(
            "{:?} changed={} unhandled={} actions={:?} status={:?} err={:?}",
            r.state_ids(),
            r.changed,
            r.unhandled,
            r.action_names(),
            r.status,
            r.error.as_ref().map(ToString::to_string)
        ),
        Err(e) => format!("error {e}"),
    };
    let Some(s) = interp.state() else { return receipt };
    let history: BTreeMap<_, _> = s.history().iter().collect();
    format!(
        "{receipt} | {:?} ctx={} hist={history:?} {:?} out={} err={:?} q={} mut={} seq={} at={:?}",
        s.configuration,
        s.context(),
        s.status(),
        s.output(),
        s.error(),
        s.internal_queue.len(),
        s.mutations,
        s.entry_seq,
        s.entered_at
    )
}

/// Run the machine with the fast path on/off; both traces must be identical.
fn differential(name: &str, config: &Value, events: &[&str]) {
    let run = |fast: bool| -> Option<Vec<String>> {
        FAST_PATH.with(|f| f.set(fast));
        let out = Machine::from_value(config).ok().map(|machine| {
            let mut interp = SyncInterpreter::new(machine, logic());
            let started = interp.start();
            let mut trace = vec![observe(&interp, &started)];
            for &e in events {
                let receipt = interp.send(e);
                trace.push(observe(&interp, &receipt));
            }
            trace
        });
        FAST_PATH.with(|f| f.set(true));
        out
    };
    let general = run(false).unwrap_or_else(|| panic!("{name}: machine does not compile"));
    let fast = run(true).unwrap();
    assert_eq!(general, fast, "{name}: fast and general paths differ for {config}");
}

/// Count the transitions each machine marks as plain (documenting the qualification claims).
fn plain_count(config: &Value) -> Option<usize> {
    let machine = Machine::from_value(config).ok()?;
    let plans = machine.plans()?;
    Some(
        (0..machine.transitions().len())
            .filter(|&i| plans.is_plain(TransitionId::from_index(i)))
            .count(),
    )
}

/// Structural cases the random generator never produces; each is differentially checked and
/// its fast-path qualification asserted (`None` expected when the machine has history states
/// and therefore no plans at all).
// One table, so every case is visible side by side during review.
#[allow(clippy::too_many_lines)]
#[test]
fn qa_fast_path_adversarial_matrix() {
    let cases: Vec<(&str, Value, Option<usize>, &[&str])> = vec![
        // A state with `after` that a transition exits: not plain.
        (
            "after_on_exited_source",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"after": {"50": "b"}, "on": {"E": "b"}},
                "b": {"on": {"E": "a"}}
            }}),
            Some(0),
            &["E", "E", "E"],
        ),
        // A state with `after` that a transition enters: not plain.
        (
            "after_on_entered_target",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"on": {"E": "b"}},
                "b": {"after": {"50": "a"}, "on": {"E": "a"}}
            }}),
            Some(0),
            &["E", "E"],
        ),
        // `after` on the root: child transitions stay plain (the root is never exited by them).
        (
            "root_after",
            json!({"id": "m", "initial": "a", "after": {"100": "a"}, "states": {
                "a": {"on": {"E": "b"}},
                "b": {"on": {"E": "a"}}
            }}),
            Some(3),
            &["E", "E", "E"],
        ),
        // A final target: not plain (finals are excluded from the fast path).
        (
            "final_on_done_target",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"on": {"E": "f"}},
                "f": {"type": "final"}
            }}),
            Some(0),
            &["E"],
        ),
        // Invoke on the exited source: not plain.
        (
            "invoke_on_exited_source",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"invoke": {"src": "svc"}, "on": {"E": "b"}},
                "b": {"on": {"E": "a"}}
            }}),
            Some(0),
            &["E", "E"],
        ),
        // A root-level handler with a root exit action: the root is NOT in the exit set
        // (the domain of a root-sourced transition is the root, whose own exit only runs
        // when the machine is done), so the step stays plain and both paths must agree
        // that the root exit action does not run.
        (
            "root_exit_actions",
            json!({"id": "m", "initial": "a", "exit": ["exit_root"], "on": {"E": "b"}, "states": {
                "a": {},
                "b": {}
            }}),
            Some(1),
            &["E"],
        ),
        // History states of either kind: no plans at all, every microstep is general.
        (
            "shallow_and_targeted_history",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "on": {"OUT": "d"}, "states": {
                    "a": {"on": {"NEXT": "b"}},
                    "b": {"on": {"NEXT": "a"}},
                    "h": {"type": "history", "history": "shallow"},
                    "ht": {"type": "history", "target": "b"}
                }},
                "d": {"on": {"BACK": "c.h"}}
            }}),
            None,
            &["NEXT", "OUT", "BACK", "NEXT"],
        ),
        // `initial` on a parallel state is ignored: child transitions stay plain.
        (
            "parallel_with_initial",
            json!({"id": "m", "type": "parallel", "initial": "r1", "states": {
                "r1": {"initial": "a", "states": {"a": {"on": {"E": "b"}}, "b": {}}},
                "r2": {"initial": "x", "states": {"x": {"on": {"E": "y"}}, "y": {}}}
            }}),
            Some(2),
            &["E", "E"],
        ),
        // A quiet root-level handler: plain (exercises the root-bit clear and the root re-entry
        // bookkeeping; the root is in both the exit range and the entry set).
        (
            "root_on_handler_quiet",
            json!({"id": "m", "initial": "a", "on": {"E": "b"}, "states": {
                "a": {},
                "b": {}
            }}),
            Some(1),
            &["E", "E"],
        ),
        // Reenter on a compound whose entry has an action: the entry action makes it not plain.
        (
            "compound_reenter_with_entry_action",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "entry": ["entry_c"], "states": {
                    "a": {"on": {"E": {"target": "#m.c", "reenter": true}}},
                    "b": {}
                }}
            }}),
            Some(0),
            &["E"],
        ),
        // An inactive state with `after` inside the exit range: conservatively not plain.
        (
            "inactive_after_in_exit_range",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "states": {
                    "a": {"on": {"E": "#m.d"}},
                    "b": {"after": {"50": "a"}}
                }},
                "d": {}
            }}),
            Some(0),
            &["E"],
        ),
        // An inactive state with an exit action inside the exit range: conservatively not plain.
        (
            "inactive_exit_action_in_range",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "states": {
                    "a": {"on": {"E": "#m.d"}},
                    "b": {"exit": ["exit_b2"]}
                }},
                "d": {}
            }}),
            Some(0),
            &["E"],
        ),
        // Multi-target into a parallel state whose regions are quiet: plain.
        (
            "parallel_target",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"on": {"E": "p"}},
                "p": {"type": "parallel", "states": {
                    "r1": {"initial": "x", "states": {"x": {"on": {"E": "y"}}, "y": {}}},
                    "r2": {"initial": "u", "states": {"u": {"on": {"E": "v"}}, "v": {}}}
                }}
            }}),
            Some(3),
            &["E", "E", "E"],
        ),
        // Five-level chain, all quiet: every transition plain.
        (
            "deep_chain_plain",
            json!({"id": "m", "initial": "a", "states": {
                "a": {"initial": "b", "on": {"E": "#m.d.e"}, "states": {
                    "b": {"initial": "c", "states": {
                        "c": {"on": {"E": "#m.d"}}
                    }}
                }},
                "d": {"initial": "e", "states": {
                    "e": {"on": {"E": "#m.a.b.c"}}
                }}
            }}),
            Some(3),
            &["E", "E", "E"],
        ),
        // Compound self-target without reenter: exits the active child, enters nothing, and
        // still must set `changed` (exit-only microstep; also what mutation A would break).
        (
            "compound_self_no_reenter",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "states": {
                    "a": {"on": {"E": "#m.c"}},
                    "b": {}
                }}
            }}),
            Some(1),
            &["E", "E"],
        ),
        // A handler on the compound itself targeting itself: domain = source, exits the
        // active child, re-enters it (preserved microstep through the fast path).
        (
            "compound_self_handler",
            json!({"id": "m", "initial": "c", "states": {
                "c": {"initial": "a", "on": {"E": "#m.c"}, "states": {
                    "a": {},
                    "b": {}
                }}
            }}),
            Some(1),
            &["E", "E"],
        ),
    ];
    for (name, config, expected, events) in cases {
        let plain = plain_count(&config);
        assert_eq!(plain, expected, "{name}: expected {expected:?} plain transitions");
        differential(name, &config, events);
    }
}
