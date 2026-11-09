//! Uses every macro under the strictest lints a user crate can enable.

#![deny(warnings)]
#![warn(clippy::pedantic)]

use xstate_statemachine::macros::{TypedEvent, TypedState, statechart};

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

/// Build the door and check the enums against it.
///
/// # Errors
/// Any engine error.
pub fn build() -> Result<xstate_statemachine::Machine, xstate_statemachine::core::Error> {
    let machine = statechart! {
        id: "door",
        initial: closed,
        states: {
            closed { on { OPEN => opened } }
            opened { on { CLOSE => closed } }
        }
    }?;
    DoorEvent::check(&machine).map_err(xstate_statemachine::core::Error::logic)?;
    Door::check(&machine).map_err(xstate_statemachine::core::Error::logic)?;
    Ok(machine)
}
