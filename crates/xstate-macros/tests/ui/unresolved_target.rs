use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opend, LOCK => #door.locked, PEEK => .inside } }
            opened { on { CLOSE => closed } }
        }
    };
}
