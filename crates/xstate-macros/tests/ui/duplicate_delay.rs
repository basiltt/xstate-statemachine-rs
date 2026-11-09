use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { after { 1000 => opened, 1000 => opened } }
            opened { on { CLOSE => closed } }
        }
    };
}
