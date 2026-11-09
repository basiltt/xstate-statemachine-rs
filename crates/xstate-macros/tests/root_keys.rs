//! The chart-root keys (`context`, `version`, `strict`, `strictTargets`, `strictConfig`,
//! `maxIterations`, `actionErrorPolicy`, `guardErrorPolicy`, `onUnhandled`) pass through to
//! `Machine::from_value` verbatim, and `strictTargets: false` relaxes target resolution the
//! same way the runtime resolver does.

use serde_json::json;
use xstate_macros::statechart;

#[test]
fn root_keys_are_forwarded_verbatim() {
    let machine = statechart! {
        id: "m",
        initial: a,
        version: "9.1",
        strict: false,
        strictTargets: false,
        strictConfig: false,
        maxIterations: 12,
        actionErrorPolicy: "rollback",
        guardErrorPolicy: "raise",
        onUnhandled: "defer",
        context: { "seed": 7 },
        states: { a { on { GO => b } }, b { type: final } }
    }
    .unwrap();
    let json = machine.to_json();
    assert_eq!(json["version"], "9.1");
    assert_eq!(json["strict"], false);
    assert_eq!(json["strictTargets"], false);
    assert_eq!(json["strictConfig"], false);
    assert_eq!(json["maxIterations"], 12);
    assert_eq!(json["actionErrorPolicy"], "rollback");
    assert_eq!(json["guardErrorPolicy"], "raise");
    assert_eq!(json["onUnhandled"], "defer");
    assert_eq!(json["context"], json!({ "seed": 7 }));
}

/// With `strictTargets` (the default), a `.child` target only resolves inside the source state;
/// with `strictTargets: false` it may also resolve against the source's parent (the runtime
/// resolver's relaxation, mirrored by the macro's compile-time checker). The strict refusal is a
/// compile error (see `tests/ui/strict_targets.rs`); here the relaxed form must compile.
#[test]
fn strict_targets_false_relaxes_child_resolution() {
    // With `strictTargets: false`, `.a2` from `a.a1` resolves against the parent `a`.
    let lax = statechart! {
        id: "m",
        strictTargets: false,
        initial: a,
        states: {
            a {
                initial: a1,
                states: { a1 { on { GO => .a2 } }, a2 {} }
            }
        }
    }
    .unwrap();
    assert_eq!(lax.to_json()["states"]["a"]["states"]["a1"]["on"]["GO"], ".a2");
}

/// A leading `crate: path,` item names the `xstate_core` path explicitly (the facade is the
/// other way the path is resolved; see `tests/renamed/`).
#[test]
fn explicit_crate_override_reaches_the_facade_path() {
    let machine = statechart! {
        crate: ::xstate_core,
        id: "m",
        initial: a,
        states: { a { on { GO => b } }, b {} }
    }
    .unwrap();
    assert_eq!(machine.id(), "m");
}
