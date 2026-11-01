//! `MachineBuilder` against the JSON parser and the interpreter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::builder::{
    Action, Branch, Guard, History, Invoke, MachineBuilder, StateBuilder, Transition,
};
use xstate_core::config::{ActionErrorPolicy, GuardErrorPolicy, MachineConfig, UnhandledPolicy};
use xstate_core::{ErrorKind, Machine, MachineLogic, SimulatedClock, SyncInterpreter};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The `machine` document of the conformance case whose file name starts with `prefix_`.
fn conformance_machine(prefix: &str) -> Value {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases");
    let wanted = format!("{prefix}_");
    let path = std::fs::read_dir(&dir)
        .expect("conformance directory")
        .map(|entry| entry.expect("dir entry").path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&wanted)))
        .unwrap_or_else(|| panic!("no conformance case {prefix}"));
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["machine"].clone()
}

/// Assert that `builder` and the JSON `doc` are interchangeable: same configuration tree,
/// same compiled structure hash, and the builder's own JSON export round-trips.
fn assert_interchangeable(doc: &Value, builder: MachineBuilder) {
    let parsed = MachineConfig::from_value(doc).expect("parse json");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let built = builder.build_config().expect("build config");

    assert_eq!(format!("{:?}", built.root), format!("{:?}", parsed.root));
    assert_eq!(built.context, parsed.context);
    assert_eq!(built.version, parsed.version);
    assert_eq!(built.policies, parsed.policies);

    let from_json = Machine::from_value(doc).expect("compile json");
    let from_builder = Machine::from_config(built).expect("compile builder");
    assert_eq!(from_builder.structure_hash(), from_json.structure_hash());

    let exported = Machine::from_value(&from_builder.to_json()).expect("re-import export");
    assert_eq!(exported.structure_hash(), from_json.structure_hash());
}

fn assert_case(prefix: &str, builder: MachineBuilder) {
    assert_interchangeable(&conformance_machine(prefix), builder);
}

// ---------------------------------------------------------------------------------------------
// Conformance machines: builder == JSON
// ---------------------------------------------------------------------------------------------

#[test]
fn toggle_basic_matches_json() {
    assert_case(
        "001",
        MachineBuilder::new("toggle")
            .initial("inactive")
            .state("inactive", |s| s.on("TOGGLE", "active"))
            .state("active", |s| s.on("TOGGLE", "inactive")),
    );
}

#[test]
fn guard_and_assign_matches_json() {
    assert_case(
        "002",
        MachineBuilder::new("door")
            .initial("closed")
            .context(json!({ "opens": 0, "locked": true }))
            .state("closed", |s| {
                s.on(
                    "UNLOCK",
                    Transition::targetless().action(Action::assign(json!({ "locked": false }))),
                )
                .on(
                    "OPEN",
                    Transition::to("opened")
                        .guard("isUnlocked")
                        .action("countOpen")
                        .action(Action::assign(json!({ "opens": 1 }))),
                )
            })
            .state("opened", |s| s.on("CLOSE", "closed")),
    );
}

#[test]
fn nested_entry_exit_order_matches_json() {
    let leaf = |s: StateBuilder, name: &str| s.entry(format!("e_{name}")).exit(format!("x_{name}"));
    assert_case(
        "004",
        MachineBuilder::new("m")
            .initial("a")
            .state("a", |a| {
                leaf(a, "a")
                    .initial("a1")
                    .state("a1", |a1| {
                        leaf(a1, "a1")
                            .initial("a1x")
                            .state("a1x", |s| leaf(s, "a1x").on("NEXT", "a1y"))
                            .state("a1y", |s| leaf(s, "a1y").on("JUMP", "#m.b.b2"))
                    })
                    .on("RESET", "b")
            })
            .state("b", |b| {
                leaf(b, "b")
                    .initial("b1")
                    .state("b1", |s| leaf(s, "b1"))
                    .state("b2", |s| leaf(s, "b2").on("BACK", "#m.a"))
            }),
    );
}

#[test]
fn parallel_regions_on_done_match_json() {
    let region = |s: StateBuilder, start: &str, event: &str, done: &str| {
        s.initial(start).state(start, |t| t.on(event, done.to_owned())).final_state(done)
    };
    assert_case(
        "005",
        MachineBuilder::new("release")
            .initial("checks")
            .parallel_state("checks", |c| {
                c.on_done("deployed")
                    .state("build", |b| region(b, "compiling", "BUILD_OK", "built"))
                    .state("lint", |l| region(l, "linting", "LINT_OK", "clean"))
            })
            .final_state("deployed"),
    );
}

#[test]
fn shallow_history_matches_json() {
    assert_case(
        "006",
        MachineBuilder::new("wizard")
            .initial("form")
            .state("form", |f| {
                f.initial("name")
                    .state("name", |s| s.on("NEXT", "address"))
                    .state("address", |a| {
                        a.initial("street").state("street", |s| s.on("NEXT", "city")).atomic("city")
                    })
                    .history("resume", History::shallow())
                    .on("ASK", "help")
            })
            .state("help", |s| s.on("BACK", "form.resume")),
    );
}

#[test]
fn history_with_default_target_matches_json() {
    assert_case(
        "008",
        MachineBuilder::new("wizard")
            .initial("intro")
            .state("intro", |s| s.on("GO", "form.resume"))
            .state("form", |f| {
                f.initial("name")
                    .atomic("name")
                    .atomic("address")
                    .history("resume", History::shallow().target("address"))
            }),
    );
}

#[test]
fn after_fixed_and_named_delays_match_json() {
    assert_case(
        "009",
        MachineBuilder::new("login")
            .initial("active")
            .state("active", |s| s.after(500_u64, "expired").on("LOGOUT", "loggedOut"))
            .final_state("expired")
            .state("loggedOut", |s| s.on("LOGIN", "active")),
    );
    assert_case(
        "011",
        MachineBuilder::new("retry")
            .initial("waiting")
            .state("waiting", |s| s.after("BACKOFF", "retrying"))
            .final_state("retrying"),
    );
}

#[test]
fn always_guard_list_matches_json() {
    assert_case(
        "012",
        MachineBuilder::new("triage")
            .initial("idle")
            .state("idle", |s| s.on("SUBMIT", "checking"))
            .state("checking", |s| {
                s.entry("startCheck").always([
                    Transition::to("rejected").guard("isBlocked"),
                    Transition::to("approved").guard("isTrusted"),
                    Transition::to("review"),
                ])
            })
            .final_state("approved")
            .final_state("rejected")
            .state("review", |s| s.on("APPROVE", "approved")),
    );
}

#[test]
fn forbidden_transition_matches_json() {
    assert_case(
        "014",
        MachineBuilder::new("f")
            .initial("p")
            .state("p", |p| {
                p.initial("c1")
                    .on("E", Transition::to("q").action("parentHandled"))
                    .state("c1", |s| s.forbid("E").on("NEXT", "c2"))
                    .atomic("c2")
            })
            .atomic("q"),
    );
}

#[test]
fn raise_and_cancel_builtins_match_json() {
    assert_case(
        "015",
        MachineBuilder::new("r")
            .initial("a")
            .state("a", |s| {
                s.on(
                    "GO",
                    Transition::to("b")
                        .action("beforeRaise")
                        .action(Action::raise("INTERNAL"))
                        .action("afterRaise"),
                )
            })
            .state("b", |s| {
                s.entry("enterB").on("INTERNAL", Transition::to("c").action("handledInternal"))
            })
            .state("c", |s| s.entry("enterC")),
    );
    let delayed = |target: &'static str| {
        move |s: StateBuilder| {
            s.entry(Action::raise("TICK").delay(200_u64).id("tick"))
                .on("TICK", Transition::to(target).action("ticked"))
                .on("STOP", Transition::targetless().action(Action::cancel("tick")))
        }
    };
    assert_case(
        "033",
        MachineBuilder::new("m")
            .initial("a")
            .state("a", delayed("b"))
            .state("b", delayed("c"))
            .atomic("c"),
    );
}

#[test]
fn final_output_and_context_match_json() {
    assert_case(
        "016",
        MachineBuilder::new("job")
            .initial("running")
            .context(json!({ "attempts": 1 }))
            .output(json!({ "ok": true }))
            .state("running", |s| s.on("FINISH", "done").on("FAIL", "failed"))
            .final_state("done")
            .final_state_with("failed", |s| s.output(json!({ "ok": false }))),
    );
}

#[test]
fn reenter_and_targetless_match_json() {
    assert_case(
        "017",
        MachineBuilder::new("s").initial("a").state("a", |s| {
            s.entry("e_a")
                .exit("x_a")
                .on("SOFT", Transition::to("a").action("soft"))
                .on("HARD", Transition::to("a").reenter().action("hard"))
                .on("TARGETLESS", Transition::targetless().action("targetless"))
        }),
    );
}

#[test]
fn invoke_done_and_error_match_json() {
    assert_case(
        "018",
        MachineBuilder::new("profile")
            .initial("idle")
            .context(json!({ "user": null }))
            .state("idle", |s| s.on("LOAD", "loading"))
            .state("loading", |s| {
                s.invoke(
                    Invoke::new("fetchUser")
                        .id("fetchUser")
                        .on_done(Transition::to("ready").action("saveUser"))
                        .on_error("failed"),
                )
            })
            .final_state("ready")
            .atomic("failed"),
    );
    assert_case(
        "019",
        MachineBuilder::new("profile")
            .initial("idle")
            .state("idle", |s| s.on("LOAD", "loading"))
            .state("loading", |s| {
                s.invoke(
                    Invoke::new("fetchUser")
                        .id("fetchUser")
                        .on_done("ready")
                        .on_error(Transition::to("failed").action("recordError")),
                )
            })
            .final_state("ready")
            .state("failed", |s| s.on("RETRY", "loading")),
    );
}

#[test]
fn choose_state_in_and_boolean_guards_match_json() {
    assert_case(
        "032",
        MachineBuilder::new("m").initial("p").parallel_state("p", |p| {
            p.state("mode", |m| {
                m.initial("day").state("day", |s| s.on("NIGHT", "night")).atomic("night")
            })
            .state("work", |w| {
                w.initial("idle")
                    .state("idle", |s| {
                        s.on(
                            "PING",
                            Transition::targetless().action(Action::choose([
                                Branch::when(Guard::state_in("#m.p.mode.night"), ["atNight"]),
                                Branch::when(
                                    Guard::and([Guard::named("isAdmin"), Guard::not("isBanned")]),
                                    ["adminByDay"],
                                ),
                                Branch::otherwise(["otherwise"]),
                            ])),
                        )
                        .on(
                            "LOCK",
                            Transition::to("locked").guard(Guard::or(["isAdmin", "isBanned"])),
                        )
                    })
                    .atomic("locked")
            })
        }),
    );
}

#[test]
fn pending_timers_machine_matches_json() {
    assert_case(
        "037",
        MachineBuilder::new("t")
            .initial("waiting")
            .state("waiting", |s| {
                s.entry(
                    Action::raise_event(json!({ "type": "NUDGE", "n": 1 }))
                        .delay(5000_u64)
                        .id("nudge"),
                )
                .after(Duration::from_secs(1), "expired")
                .after("BACKOFF", Transition::to("retrying").action("backoff"))
                .on("NUDGE", Transition::targetless().action("nudged"))
            })
            .final_state("expired")
            .atomic("retrying"),
    );
}

// ---------------------------------------------------------------------------------------------
// Root options, metadata and policies
// ---------------------------------------------------------------------------------------------

#[test]
fn policies_metadata_and_custom_ids_match_json() {
    let doc = json!({
        "id": "m",
        "version": "1.2",
        "context": { "n": 0 },
        "initial": "a",
        "strict": true,
        "strictTargets": false,
        "maxIterations": 7,
        "actionErrorPolicy": "rollback",
        "guardErrorPolicy": "raise",
        "onUnhandled": "defer",
        "description": "root",
        "tags": ["t1", "t2"],
        "meta": { "owner": "ops" },
        "states": {
            "a": {
                "id": "alpha",
                "description": "first",
                "meta": { "x": 1 },
                "on": {
                    "GO": { "target": "#alpha", "reenter": true, "description": "loop",
                            "meta": { "k": "v" } },
                    "MANY": ["b", { "target": "a", "guard": "!blocked" }],
                    "SKIP": { "actions": [] },
                    "*": "b",
                    "user.*": { "target": "b", "guard": { "type": "min", "params": { "n": 3 } } }
                }
            },
            "b": { "type": "final" }
        }
    });
    let builder = MachineBuilder::new("m")
        .version("1.2")
        .context(json!({ "n": 0 }))
        .initial("a")
        .strict(true)
        .strict_targets(false)
        .max_iterations(7)
        .action_error_policy(ActionErrorPolicy::Rollback)
        .guard_error_policy(GuardErrorPolicy::Raise)
        .on_unhandled(UnhandledPolicy::Defer)
        .description("root")
        .tags(["t1", "t2"])
        .meta(json!({ "owner": "ops" }))
        .state("a", |a| {
            a.id("alpha")
                .description("first")
                .meta(json!({ "x": 1 }))
                .on(
                    "GO",
                    Transition::to("#alpha")
                        .reenter()
                        .description("loop")
                        .meta(json!({ "k": "v" })),
                )
                .on("MANY", [Transition::to("b"), Transition::to("a").guard("!blocked")])
                .on("SKIP", Transition::targetless())
                .on("*", "b")
                .on(
                    "user.*",
                    Transition::to("b").guard(Guard::with_params("min", json!({ "n": 3 }))),
                )
        })
        .final_state("b");
    assert_interchangeable(&doc, builder);
}

#[test]
fn typed_context_serialises_like_json() {
    #[derive(serde::Serialize)]
    struct Cart {
        items: Vec<String>,
        total: u32,
    }
    let config = MachineBuilder::new("m")
        .context(Cart { items: vec!["a".into()], total: 3 })
        .atomic("only")
        .build_config()
        .unwrap();
    assert_eq!(config.context, json!({ "items": ["a"], "total": 3 }));
    assert_eq!(config.root.initial.as_deref(), Some("only"));
}

#[test]
fn builder_export_round_trips_through_json() {
    let machine = MachineBuilder::new("m")
        .initial("a")
        .state("a", |s| {
            s.entry(Action::assign(json!({ "n": 1 })))
                .on("GO", [Transition::to("b").guard("g").reenter(), Transition::to("a")])
                .after("WAIT", "b")
        })
        .state("b", |s| s.invoke(Invoke::new("svc").input(json!({ "k": 1 })).system_id("sys")))
        .state("c", |c| c.initial("c1").atomic("c1").history("h", History::deep()))
        .build()
        .unwrap();
    let json = machine.to_json();
    assert_eq!(json["states"]["a"]["entry"]["type"], "assign");
    let again = Machine::from_value(&json).unwrap();
    assert_eq!(again.structure_hash(), machine.structure_hash());
}

// ---------------------------------------------------------------------------------------------
// Running builder machines
// ---------------------------------------------------------------------------------------------

#[test]
fn toggle_runs_through_the_sync_interpreter() {
    let machine = MachineBuilder::new("toggle")
        .initial("inactive")
        .state("inactive", |s| s.on("TOGGLE", "active"))
        .state("active", |s| s.on("TOGGLE", "inactive"))
        .build()
        .unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().unwrap();
    assert_eq!(interp.state_ids(), ["toggle.inactive"]);
    interp.send("TOGGLE").unwrap();
    assert_eq!(interp.state_ids(), ["toggle.active"]);
    interp.send("TOGGLE").unwrap();
    assert_eq!(interp.state_ids(), ["toggle.inactive"]);
}

#[test]
fn guards_and_assign_run_through_the_interpreter() {
    let machine = MachineBuilder::new("door")
        .initial("closed")
        .context(json!({ "opens": 0, "locked": true }))
        .state("closed", |s| {
            s.on(
                "UNLOCK",
                Transition::targetless().action(Action::assign(json!({ "locked": false }))),
            )
            .on(
                "OPEN",
                Transition::to("opened")
                    .guard("isUnlocked")
                    .action(Action::assign(json!({ "opens": 1 }))),
            )
        })
        .state("opened", |s| s.on("CLOSE", "closed"))
        .build()
        .unwrap();
    let logic = MachineLogic::dynamic().guard("isUnlocked", |ctx, _, _| Ok(ctx["locked"] == false));
    let mut interp = SyncInterpreter::new(machine, logic);
    interp.start().unwrap();
    interp.send("OPEN").unwrap();
    assert_eq!(interp.state_ids(), ["door.closed"], "guard blocks while locked");
    interp.send("UNLOCK").unwrap();
    interp.send("OPEN").unwrap();
    assert_eq!(interp.state_ids(), ["door.opened"]);
    assert_eq!(interp.context().unwrap()["opens"], 1);
}

#[test]
fn nested_history_and_after_run_through_the_interpreter() {
    let machine = MachineBuilder::new("wizard")
        .initial("form")
        .state("form", |f| {
            f.initial("name")
                .state("name", |s| s.on("NEXT", "address"))
                .atomic("address")
                .history("resume", History::shallow())
                .on("ASK", "help")
        })
        .state("help", |s| s.on("BACK", "form.resume").after(100_u64, "form.name"))
        .build()
        .unwrap();
    let clock = Arc::new(SimulatedClock::new());
    let mut interp =
        SyncInterpreter::with_clock(machine, MachineLogic::dynamic(), Arc::clone(&clock) as _);
    interp.start().unwrap();
    interp.send("NEXT").unwrap();
    interp.send("ASK").unwrap();
    assert_eq!(interp.state_ids(), ["wizard.help"]);
    interp.send("BACK").unwrap();
    assert_eq!(interp.state_ids(), ["wizard.form.address"], "history restored the child");
    interp.send("ASK").unwrap();
    interp.advance(&clock, 100.0).unwrap();
    assert_eq!(interp.state_ids(), ["wizard.form.name"], "after fired");
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn config_error(builder: MachineBuilder) -> (ErrorKind, String) {
    let err = builder.build().expect_err("expected an error");
    (err.kind(), err.to_string())
}

#[test]
fn compound_state_without_initial_is_an_error() {
    let (kind, message) = config_error(
        MachineBuilder::new("m").initial("form").state("form", |f| f.atomic("a").atomic("b")),
    );
    assert_eq!(kind, ErrorKind::InvalidConfig);
    assert!(message.contains("states.form.initial"), "{message}");

    // The root counts too.
    let (kind, message) = config_error(MachineBuilder::new("m").atomic("a").atomic("b"));
    assert_eq!(kind, ErrorKind::InvalidConfig);
    assert!(message.contains("initial"), "{message}");
}

#[test]
fn single_child_initial_is_inferred() {
    let config = MachineBuilder::new("m").atomic("only").build_config().unwrap();
    assert_eq!(config.root.initial.as_deref(), Some("only"));
}

#[test]
fn unknown_target_is_an_error() {
    let (kind, message) = config_error(
        MachineBuilder::new("m").initial("a").state("a", |s| s.on("GO", "nowhere")).atomic("b"),
    );
    assert_eq!(kind, ErrorKind::StateNotFound);
    assert!(message.contains("nowhere"), "{message}");
}

#[test]
fn unknown_initial_child_is_an_error() {
    let (kind, _) = config_error(MachineBuilder::new("m").initial("ghost").atomic("a"));
    assert_eq!(kind, ErrorKind::InvalidConfig);
}

#[test]
fn duplicate_and_empty_names_are_errors() {
    let cases = [
        MachineBuilder::new("m").initial("a").atomic("a").atomic("a"),
        MachineBuilder::new("m").initial("a").atomic("a").state("", |s| s),
        MachineBuilder::new("").atomic("a"),
        MachineBuilder::new("m").initial("a").state("a", |s| s.on("X", "a").on("X", "a")),
        MachineBuilder::new("m").initial("a").state("a", |s| s.after(5_u64, "a").after(5_u64, "a")),
        MachineBuilder::new("m").initial("a").state("a", |s| s.entry("")),
        MachineBuilder::new("m")
            .initial("a")
            .state("a", |s| s.on("X", Transition::to("a").guard(Guard::not("")))),
        MachineBuilder::new("m").initial("a").state("a", |s| s.invoke(Invoke::new(""))),
        MachineBuilder::new("m").initial("a").state("a", |s| s.id("")),
        MachineBuilder::new("m").atomic("a").max_iterations(0),
    ];
    for (i, builder) in cases.into_iter().enumerate() {
        let err = builder.build_config().expect_err(&format!("case {i} should fail"));
        assert_eq!(err.kind(), ErrorKind::InvalidConfig, "case {i}: {err}");
    }
}

#[test]
fn non_object_meta_and_unserialisable_values_are_errors() {
    let err = MachineBuilder::new("m")
        .atomic("a")
        .state("b", |s| s.meta("not an object"))
        .build_config()
        .unwrap_err();
    assert!(err.to_string().contains("states.b.meta"), "{err}");

    // JSON object keys must be strings, so this map cannot be serialised.
    let bad: std::collections::HashMap<(u8, u8), u8> =
        std::collections::HashMap::from([((1, 2), 3)]);
    let err = MachineBuilder::new("m").context(bad).atomic("a").build_config().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidConfig);
    assert!(err.to_string().contains("context"), "{err}");
}

#[test]
fn builder_can_be_cloned_as_a_template() {
    let template = MachineBuilder::new("m").initial("a").atomic("a");
    let first = template.clone().context(json!({ "n": 1 })).build_config().unwrap();
    let second = template.build_config().unwrap();
    assert_eq!(first.context, json!({ "n": 1 }));
    assert_eq!(second.context, Value::Null);
}
