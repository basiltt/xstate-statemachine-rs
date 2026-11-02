//! `statechart!` expansion: every construct, compared against the hand-written XState JSON it
//! must produce, then run once through the `SyncInterpreter`.

use std::sync::Arc;

use serde_json::json;
use xstate_core::{Machine, MachineLogic, SimulatedClock, SyncInterpreter};
use xstate_macros::statechart;

fn player(max_plays: u32) -> Result<Machine, xstate_core::Error> {
    statechart! {
        id: "player",
        version: "1.2",
        strict: false,
        context: { "plays": 0, "max": max_plays },
        initial: stopped,
        description: "A media player",
        entry: [boot],
        on {
            RESET => #player.stopped [actions: [assign({ "plays": 0 })], reenter],
            debug.* => none [actions: log],
            "skip-intro" => #player.playing.track.body [reenter: false],
        },
        states: {
            stopped {
                tags: [idle, "quiet"],
                on {
                    PLAY => playing [guard: hasMedia, actions: [countPlay], description: "start"]
                        | error,
                    PLAY_MUTED => (#player.playing.track.body, #mute),
                    LOAD => loading [guard: inRange({ "min": 1 }), meta: { "ui": "button" }],
                },
            },
            playing {
                type: parallel,
                entry: startClock,
                exit: [stopClock],
                onDone => stopped,
                on { SKIP => .track.outro, STOP => stopped },
                states: {
                    track {
                        initial: intro,
                        states: {
                            intro { after { 1000 => body, slowFade => outro } }
                            body {
                                on { NEXT => outro [guard: !isLast] | intro },
                                meta: { "bars": 32 },
                            }
                            outro { type: final, output: { "done": true } }
                            resume { type: history(deep), target: body }
                        }
                    }
                    volume {
                        initial: normal,
                        states: {
                            normal { on { MUTE => muted } },
                            muted { id: "mute", on { UNMUTE => normal } },
                        }
                    }
                }
            }
            loading {
                invoke {
                    src: fetchMedia,
                    id: fetcher,
                    input: { "url": "https://example.test" },
                    systemId: loader,
                    onDone => stopped,
                    onError => error [actions: [logError]],
                }
                always => error [guard: !inRange({ "min": 2 })],
            }
            error { on { RETRY => loading }, exit: [] }
            off { type: final, description: "powered down" }
            idle
        }
    }
}

#[test]
fn expansion_matches_the_hand_written_json() {
    let machine = player(3).unwrap();
    let expected = json!({
        "id": "player",
        "version": "1.2",
        "strict": false,
        "context": { "plays": 0, "max": 3 },
        "initial": "stopped",
        "description": "A media player",
        "entry": ["boot"],
        "on": {
            "RESET": {
                "target": "#player.stopped",
                "actions": [{ "type": "assign", "params": { "plays": 0 } }],
                "reenter": true
            },
            "debug.*": { "actions": ["log"] },
            "skip-intro": { "target": "#player.playing.track.body", "reenter": false }
        },
        "states": {
            "stopped": {
                "tags": ["idle", "quiet"],
                "on": {
                    "PLAY": [
                        {
                            "target": "playing",
                            "guard": "hasMedia",
                            "actions": ["countPlay"],
                            "description": "start"
                        },
                        "error"
                    ],
                    "PLAY_MUTED": { "target": ["#player.playing.track.body", "#mute"] },
                    "LOAD": {
                        "target": "loading",
                        "guard": { "type": "inRange", "params": { "min": 1 } },
                        "meta": { "ui": "button" }
                    }
                }
            },
            "playing": {
                "type": "parallel",
                "entry": ["startClock"],
                "exit": ["stopClock"],
                "onDone": "stopped",
                "on": { "SKIP": ".track.outro", "STOP": "stopped" },
                "states": {
                    "track": {
                        "initial": "intro",
                        "states": {
                            "intro": { "after": { "1000": "body", "slowFade": "outro" } },
                            "body": {
                                "on": { "NEXT": [{ "target": "outro", "guard": "!isLast" }, "intro"] },
                                "meta": { "bars": 32 }
                            },
                            "outro": { "type": "final", "output": { "done": true } },
                            "resume": { "type": "history", "history": "deep", "target": "body" }
                        }
                    },
                    "volume": {
                        "initial": "normal",
                        "states": {
                            "normal": { "on": { "MUTE": "muted" } },
                            "muted": { "id": "mute", "on": { "UNMUTE": "normal" } }
                        }
                    }
                }
            },
            "loading": {
                "invoke": {
                    "src": "fetchMedia",
                    "id": "fetcher",
                    "input": { "url": "https://example.test" },
                    "systemId": "loader",
                    "onDone": "stopped",
                    "onError": { "target": "error", "actions": ["logError"] }
                },
                "always": {
                    "target": "error",
                    "guard": {
                        "type": "not",
                        "params": { "guard": { "type": "inRange", "params": { "min": 2 } } }
                    }
                }
            },
            "error": { "on": { "RETRY": "loading" }, "exit": [] },
            "off": { "type": "final", "description": "powered down" },
            "idle": {}
        }
    });
    assert_eq!(machine.to_json(), expected);
}

#[test]
fn the_expanded_machine_runs() {
    let machine = player(3).unwrap();
    let noop = |_: &mut xstate_core::ActionScope<'_, serde_json::Value>| Ok(());
    let logic = MachineLogic::dynamic()
        .action("boot", noop)
        .action("countPlay", noop)
        .action("startClock", noop)
        .action("stopClock", noop)
        .action("log", noop)
        .action("logError", noop)
        .guard("hasMedia", |_, _, _| Ok(true))
        .guard("inRange", |_, _, _| Ok(true))
        .service("fetchMedia", |_, input| Ok(input.clone()))
        .guard("isLast", |_, _, _| Ok(false))
        .delay("slowFade", 5000.0);
    let clock = Arc::new(SimulatedClock::new());
    let mut interp = SyncInterpreter::with_clock(machine, logic, Arc::clone(&clock) as _);
    interp.start().unwrap();
    assert_eq!(interp.state_ids(), ["player.stopped"]);
    interp.send("PLAY").unwrap();
    assert_eq!(interp.state_ids(), ["player.playing.track.intro", "player.playing.volume.normal"]);
    interp.send("MUTE").unwrap();
    assert!(interp.matches("mute"));
    interp.advance(&clock, 1000.0).unwrap();
    assert!(interp.matches("player.playing.track.body"));
    interp.send("NEXT").unwrap();
    assert!(interp.matches("player.playing.track.outro"));
    assert_eq!(interp.context().unwrap()["max"], 3);
}

#[test]
fn bare_states_and_trailing_commas_are_accepted() {
    let machine = statechart! {
        id: "toggle",
        initial: "inactive",
        states: { inactive { on { TOGGLE => active, }, }, active { on { TOGGLE => inactive } }, },
    }
    .unwrap();
    assert_eq!(
        machine.to_json(),
        json!({
            "id": "toggle",
            "initial": "inactive",
            "states": {
                "inactive": { "on": { "TOGGLE": "active" } },
                "active": { "on": { "TOGGLE": "inactive" } }
            }
        })
    );
}

#[test]
fn runtime_rules_still_surface_as_errors() {
    // `maxIterations` must be positive: a runtime rule the macro passes through untouched.
    let err = statechart! { id: "m", maxIterations: 0, states: { a } }.unwrap_err();
    assert_eq!(err.kind(), xstate_core::ErrorKind::InvalidConfig);
}
