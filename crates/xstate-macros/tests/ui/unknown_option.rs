use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opened [gaurd: isUnlocked] } }
            opened { on { CLOSE => closed } }
        }
    };
}
