use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { context: { "n": 0 } }
            opened { on { CLOSE => closed } }
        }
    };
}
