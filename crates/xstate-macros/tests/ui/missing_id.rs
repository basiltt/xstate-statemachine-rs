use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        initial: closed,
        states: { closed { on { OPEN => opened } }, opened {} }
    };
}
