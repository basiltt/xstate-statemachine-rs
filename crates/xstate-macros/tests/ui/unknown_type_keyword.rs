use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { type: bogus }
            opened { on { CLOSE => closed } }
        }
    };
}
