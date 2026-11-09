use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { id: "shared", on { OPEN => opened } }
            opened { id: "shared", on { CLOSE => closed } }
        }
    };
}
