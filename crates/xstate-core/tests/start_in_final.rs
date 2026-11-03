//! Starting directly in a top-level final state runs the exit actions, as XState does
//! (`e_root, e_done, x_done, x_root`; verified against xstate 5.33.3).

use xstate_core::{Machine, MachineLogic, Status, SyncInterpreter};

#[test]
fn start_in_a_final_state_runs_every_exit_action() {
    let machine = Machine::from_json(
        r#"{"id":"m","initial":"done","entry":"e_root","exit":"x_root",
            "states":{"done":{"type":"final","entry":"e_done","exit":"x_done"}}}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let logic = ["e_root", "x_root", "e_done", "x_done"]
        .into_iter()
        .fold(MachineLogic::dynamic(), |l, name| l.action(name, |_| Ok(())));
    let mut interp = SyncInterpreter::new(machine, logic);
    let receipt = interp.start().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(receipt.action_names(), ["e_root", "e_done", "x_done", "x_root"]);
    assert_eq!(interp.status(), Status::Done);
}
