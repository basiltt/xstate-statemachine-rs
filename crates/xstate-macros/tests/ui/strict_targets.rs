use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "m",
        initial: a,
        states: {
            a {
                initial: a1,
                states: { a1 { on { GO => .a2 } }, a2 {} }
            }
        }
    };
}
