use xstate_macros::TypedEvent;

#[derive(TypedEvent)]
enum Event<T> {
    Go(T),
}

fn main() {}
