//! Strongly typed indices into a compiled [`Machine`](crate::machine::Machine).
//!
//! Every id is a `u32` newtype so that a state index can never be confused with a transition
//! index at compile time, and so that the hot path stays allocation-free.

use std::fmt;

macro_rules! id_newtype {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub(crate) u32);

        impl $name {
            /// Position of this item in its arena.
            #[inline]
            #[must_use]
            pub const fn index(self) -> usize {
                self.0 as usize
            }

            #[inline]
            pub(crate) const fn from_index(index: usize) -> Self {
                // Arenas are bounded far below u32::MAX by the configuration limits.
                #[allow(clippy::cast_possible_truncation)]
                Self(index as u32)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

id_newtype! {
    /// Index of a state node in the machine arena. The root is always `StateId(0)`.
    StateId
}

id_newtype! {
    /// Index of a transition in the machine's transition table.
    TransitionId
}

impl StateId {
    /// The machine root.
    pub const ROOT: StateId = StateId(0);
}
