use xstate_macros::TypedState;

#[derive(TypedState)]
#[state(machine = "door")]
enum Door {
    #[state(id = "door.closed")]
    Closed,
    #[state(id = "door.closed")]
    Shut,
}

fn main() {}
