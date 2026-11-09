//! Depends on the facade under the name `myfacade`; the macros must find `xstate_core` and
//! `serde_json` through it, not through a literal `xstate_statemachine`.

use myfacade::macros::{TypedEvent, TypedState, statechart};

/// Events of the door.
#[derive(TypedEvent)]
#[event(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DoorEvent {
    /// Open.
    Open,
    /// Close.
    Close,
}

/// Leaf states of the door.
#[derive(Debug, Clone, Copy, PartialEq, TypedState)]
#[state(machine = "door")]
pub enum Door {
    /// Closed.
    Closed,
    /// Opened.
    Opened,
}

/// Build the door under the renamed facade.
///
/// # Errors
/// Any engine error.
pub fn build() -> Result<myfacade::Machine, myfacade::core::Error> {
    let machine = statechart! {
        id: "door",
        initial: closed,
        context: { "max": 3 },
        states: {
            closed { on { OPEN => opened } }
            opened { on { CLOSE => closed } }
        }
    }?;
    DoorEvent::check(&machine).map_err(myfacade::core::Error::logic)?;
    Door::check(&machine).map_err(myfacade::core::Error::logic)?;
    Ok(machine)
}
