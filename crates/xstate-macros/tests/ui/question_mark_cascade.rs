//! A failing `statechart!` must not add a second error when followed by `?`.

use xstate_macros::statechart;

fn build() -> Result<(), xstate_core::Error> {
    let _machine = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opend } }
            opened {}
        }
    }?;
    Ok(())
}

fn main() {
    let _ = build();
}
