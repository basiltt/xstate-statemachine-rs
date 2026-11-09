use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { type: history(nowhere) }
            opened { on { CLOSE => closed } }
        }
    };
}
