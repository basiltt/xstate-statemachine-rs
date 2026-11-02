use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on_done => opened }
            opened {}
        }
    };
}
