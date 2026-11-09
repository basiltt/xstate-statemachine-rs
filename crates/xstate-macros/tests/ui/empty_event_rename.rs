use xstate_macros::TypedEvent;

#[derive(TypedEvent)]
enum Door {
    #[event(rename = "")]
    Open,
}

fn main() {}
