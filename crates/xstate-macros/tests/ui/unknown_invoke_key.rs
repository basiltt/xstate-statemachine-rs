use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { invoke { src: fetch, bogus: 1 } }
            opened { on { CLOSE => closed } }
        }
    };
}
