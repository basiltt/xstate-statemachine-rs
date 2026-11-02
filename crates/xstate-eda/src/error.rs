//! Error types of the crate.
//!
//! | Python | Rust |
//! |--------|------|
//! | `EnvelopeCorruptError` | [`EnvelopeError::Corrupt`] |
//! | `EnvelopeTooLargeError` (a subclass of the above) | [`EnvelopeError::TooLarge`] |
//! | `BrokerPublishError` | [`BrokerError::Publish`] |
//! | `ReplayRefusedError` | [`EdaError::ReplayRefused`] |
//! | `ValueError` / `TypeError` on bad arguments | [`EdaError::InvalidArgument`] |
//! | `ValueError` on a malformed `meta.publish` | [`EdaError::InvalidSpec`] |

use xstate_persistence::StoreError;

/// An envelope failed validation or (de)serialisation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvelopeError {
    /// The shape is invalid: a missing or mistyped attribute, a forbidden or malformed
    /// extension, a bad `traceparent`, `data` nested deeper than
    /// [`MAX_DATA_DEPTH`](crate::envelope::MAX_DATA_DEPTH), non-object `data` where an event
    /// payload is required, or text that is not JSON (Python `EnvelopeCorruptError`).
    #[error("{0}")]
    Corrupt(String),
    /// The encoded envelope exceeds the byte cap, checked before parsing (Python
    /// `EnvelopeTooLargeError`, a subclass of `EnvelopeCorruptError`).
    #[error("{0}")]
    TooLarge(String),
}

impl EnvelopeError {
    /// The Python class name of this error (`EnvelopeCorruptError` / `EnvelopeTooLargeError`),
    /// as written into dead-letter error chains.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Corrupt(_) => "EnvelopeCorruptError",
            Self::TooLarge(_) => "EnvelopeTooLargeError",
        }
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }
}

/// A broker operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BrokerError {
    /// The broker refused or could not take a publish (Python `BrokerPublishError`, the
    /// failure `fail_next_publish()` injects by default).
    #[error("{0}")]
    Publish(String),
    /// The envelope could not cross the wire (too large, not encodable).
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// Any other broker failure.
    #[error("{0}")]
    Other(String),
}

impl BrokerError {
    /// The Python class name of this error, as written into dead-letter error chains.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Publish(_) => "BrokerPublishError",
            Self::Envelope(e) => e.type_name(),
            Self::Other(_) => "ConnectionError",
        }
    }
}

/// Errors of the stores, relays, dispatcher utilities and document generators.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EdaError {
    /// An envelope was invalid.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// A broker call failed.
    #[error(transparent)]
    Broker(#[from] BrokerError),
    /// A store backend failed (SQLite errors are [`StoreError::Unavailable`]).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A dead-letter replay was refused (Python `ReplayRefusedError`).
    #[error("{0}")]
    ReplayRefused(String),
    /// An argument was out of range (Python `ValueError` / `TypeError`).
    #[error("{0}")]
    InvalidArgument(String),
    /// A chart's `meta.publish` is malformed (Python `ValueError`).
    #[error("{0}")]
    InvalidSpec(String),
    /// `drain()` handled its limit and the topics are still not empty (Python `RuntimeError`).
    #[error(
        "drain() handled {handled} envelopes and the topics are still not empty (a handler feeding itself?)"
    )]
    DrainLimit {
        /// Envelopes handled before giving up.
        handled: usize,
    },
    /// A fake-broker `drain()` handler failed; the delivery was nacked without requeue.
    #[error("handler failed: {0}")]
    Handler(String),
    /// An AsyncAPI document failed validation; one message per problem.
    #[error("invalid AsyncAPI document: {}", .0.join("; "))]
    InvalidAsyncApi(Vec<String>),
}

/// Result alias of the crate.
pub type Result<T, E = EdaError> = std::result::Result<T, E>;

/// Map a SQLite failure to the store error taxonomy (as `xstate-persistence` does).
#[cfg(feature = "sqlite")]
#[allow(clippy::needless_pass_by_value)] // used as a `map_err` adapter, which passes by value
pub(crate) fn sql_err(error: rusqlite::Error) -> EdaError {
    EdaError::Store(StoreError::Unavailable(format!("sqlite: {error}")))
}
