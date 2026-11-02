use xstate_macros::{TypedEvent, TypedState};

#[derive(TypedEvent)]
enum Duplicate {
    Open,
    #[event(rename = "Open")]
    AlsoOpen,
}

#[derive(TypedEvent)]
enum TwoValues {
    Pair(u8, u8),
}

#[derive(TypedState)]
enum NoMachine {
    Closed,
}

#[derive(TypedEvent)]
#[event(rename_all = "Train-Case")]
enum BadCase {
    Open,
}

fn main() {}
