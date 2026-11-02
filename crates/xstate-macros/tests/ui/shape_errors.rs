use xstate_macros::statechart;

fn main() {
    let _ = statechart! {
        id: "door",
        initial: shut,
        states: {
            closed {
                onDone => opened,
                target: opened,
            }
            opened {
                type: final,
                states: { ajar {} }
            }
            broken {
                states: { left {}, right {} }
            }
        }
    };
}
