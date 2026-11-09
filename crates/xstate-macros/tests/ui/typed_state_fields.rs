use xstate_macros::TypedState;

#[derive(TypedState)]
#[state(machine = "door")]
enum Door {
    Open(u8),
}

fn main() {}
