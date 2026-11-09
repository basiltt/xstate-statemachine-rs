use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: shut,
        states: {
            closed { on { OPEN => opened } }
            opened { on { CLOSE => closed } }
        }
    };
}
