use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        id: "again",
        states: { closed {} }
    };
}
