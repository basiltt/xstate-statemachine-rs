//! `#[derive(TypedState)]`: ids, lookups, activity and the chart check.

use xstate_core::{Machine, MachineLogic, SyncInterpreter, TypedState};
use xstate_macros::{TypedState, statechart};

fn door() -> xstate_core::Result<Machine> {
    statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opened, LOCK => locked } }
            opened { on { CLOSE => closed } }
            locked {
                initial: bolted,
                states: { bolted { on { UNBOLT => latched } }, latched { on { UNLOCK => #door.closed } } }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedState)]
#[state(machine = "door")]
enum Door {
    Closed,
    Opened,
    #[state(id = "door.locked.bolted")]
    Bolted,
    #[state(id = "door.locked.latched")]
    Latched,
}

#[test]
fn ids_and_lookups() {
    assert_eq!(Door::Closed.id(), "door.closed");
    assert_eq!(Door::Bolted.id(), "door.locked.bolted");
    assert_eq!(Door::from_id("door.opened"), Some(Door::Opened));
    assert_eq!(Door::from_id("door.locked"), None);
    assert_eq!(Door::all(), [Door::Closed, Door::Opened, Door::Bolted, Door::Latched]);
    assert_eq!(<Door as TypedState>::all().len(), 4);
    assert_eq!(<Door as TypedState>::from_id("door.closed"), Some(Door::Closed));
}

#[test]
fn check_accepts_an_exact_cover() {
    assert_eq!(Door::check(&door().unwrap()), Ok(()));
}

#[test]
fn is_active_follows_the_interpreter() {
    let mut interp = SyncInterpreter::new(door().unwrap(), MachineLogic::dynamic());
    interp.start().unwrap();
    assert!(Door::Closed.is_active(&interp));
    interp.send("LOCK").unwrap();
    let active: Vec<Door> = Door::all().iter().copied().filter(|s| s.is_active(&interp)).collect();
    assert_eq!(active, [Door::Bolted]);
    assert!(<Door as TypedState>::is_active(&Door::Bolted, &interp));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedState)]
#[state(machine = "door", rename_all = "camelCase")]
enum Drifted {
    Closed,
    #[state(id = "door.locked")]
    Locked,
    WideOpen,
}

#[test]
fn check_reports_unknown_non_leaf_and_missing_states() {
    assert_eq!(Drifted::WideOpen.id(), "door.wideOpen");
    let report = Drifted::check(&door().unwrap()).unwrap_err();
    assert_eq!(
        report,
        "machine `door`: states unknown to the chart: door.wideOpen; states that are not \
         leaves: door.locked; states missing from the enum: door.opened, door.locked.bolted, \
         door.locked.latched"
    );
}
