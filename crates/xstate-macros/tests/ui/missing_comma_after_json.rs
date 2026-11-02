use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        context: { "opens": 0 }
        states: {
            closed {}
        }
    };
}
