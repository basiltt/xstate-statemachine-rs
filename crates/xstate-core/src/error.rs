//! Error taxonomy of the core crate.
//!
//! The variants mirror the Python library's exception classes so that documentation and
//! receipts map one-to-one; [`ErrorKind`] is the stable, serialisable classification used by
//! receipts and HTTP adapters.

use std::fmt;

/// Stable classification of an [`Error`], independent of its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// The machine configuration is malformed or violates a structural rule.
    InvalidConfig,
    /// A transition targets the machine root.
    RootTarget,
    /// A state id or target does not resolve.
    StateNotFound,
    /// A named action, guard, service or delay has no implementation.
    ImplementationMissing,
    /// An event could not be normalised or failed its schema.
    InvalidEvent,
    /// An undeclared event was sent to a strict machine.
    UnknownEvent,
    /// The interpreter is not in a state that allows the operation.
    InvalidLifecycle,
    /// An action implementation failed and the policy turned it into an error.
    TransitionFailed,
    /// The eventless / self-generated event budget was exhausted.
    RunawayChain,
    /// A service or actor could not be started.
    ActorSpawning,
    /// An addressed event (`sendTo`, `forwardTo`, `sendParent`, `escalate`) reached no live
    /// actor. Python: `UnresolvedTargetError`, a subclass of `ActorSpawningError`.
    UnresolvedTarget,
    /// Payload size, depth or count limit exceeded.
    LimitExceeded,
    /// Snapshot problems (version, drift, corruption).
    Snapshot,
    /// Anything raised by user logic.
    Logic,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InvalidConfig => "invalid_config",
            Self::RootTarget => "root_target",
            Self::StateNotFound => "state_not_found",
            Self::ImplementationMissing => "implementation_missing",
            Self::InvalidEvent => "invalid_event",
            Self::UnknownEvent => "unknown_event",
            Self::InvalidLifecycle => "invalid_lifecycle",
            Self::TransitionFailed => "transition_failed",
            Self::RunawayChain => "runaway_chain",
            Self::ActorSpawning => "actor_spawning",
            Self::UnresolvedTarget => "unresolved_target",
            Self::LimitExceeded => "limit_exceeded",
            Self::Snapshot => "snapshot",
            Self::Logic => "logic",
        };
        f.write_str(s)
    }
}

/// Error type of the core crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is malformed. `path` is a JSON-pointer-like location.
    #[error("invalid machine config at {path}: {message}")]
    InvalidConfig {
        /// Location inside the configuration document.
        path: String,
        /// Human-readable problem.
        message: String,
    },
    /// A transition targets the machine root, which is never legal.
    #[error("transition from '{from}' targets the machine root")]
    RootTarget {
        /// Id of the transition's source state.
        from: String,
    },
    /// A target or state id did not resolve.
    #[error("state '{target}' not found (referenced from '{from}')")]
    StateNotFound {
        /// The unresolved reference.
        target: String,
        /// Where it was referenced.
        from: String,
    },
    /// A named implementation is missing from the logic registry.
    #[error("no {kind} implementation named '{name}'")]
    ImplementationMissing {
        /// `action`, `guard`, `service` or `delay`.
        kind: &'static str,
        /// The missing name.
        name: String,
    },
    /// The event is malformed.
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    /// An undeclared event was sent to a strict machine.
    #[error("event '{event}' is not declared anywhere in machine '{machine}'")]
    UnknownEvent {
        /// Offending event type.
        event: String,
        /// Machine id.
        machine: String,
    },
    /// Operation not valid for the interpreter's current status.
    #[error("interpreter is {status}: {message}")]
    InvalidLifecycle {
        /// Current status name.
        status: &'static str,
        /// What was attempted.
        message: String,
    },
    /// An action failed under `actionErrorPolicy: "fail"`.
    #[error("action '{action}' failed: {source}")]
    TransitionFailed {
        /// Name of the failing action.
        action: String,
        /// The underlying logic error.
        #[source]
        source: Box<Error>,
    },
    /// `maxIterations` exhausted.
    #[error("machine '{machine}' exceeded maxIterations ({limit}) while processing '{event}'")]
    RunawayChain {
        /// Machine id.
        machine: String,
        /// The configured budget.
        limit: u32,
        /// Event being processed when the budget ran out.
        event: String,
    },
    /// A service or child actor could not be started.
    #[error("cannot spawn actor '{id}': {message}")]
    ActorSpawning {
        /// Invoke or actor id.
        id: String,
        /// Reason.
        message: String,
    },
    /// An addressed event reached no live actor; it was dropped. The step that sent it still
    /// committed (its other actions ran), but is reported as failed with this error.
    #[error(
        "{action} target '{target}' did not resolve to a live actor; '{event}' was not delivered"
    )]
    UnresolvedTarget {
        /// `sendTo`, `forwardTo`, `sendParent` or `escalate`.
        action: &'static str,
        /// The addressed actor (`<no parent>` for `sendParent` / `escalate` on a root).
        target: String,
        /// Type of the dropped event.
        event: String,
    },
    /// A size, depth or count limit was exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// A snapshot could not be produced or restored.
    #[error("snapshot error: {0}")]
    Snapshot(String),
    /// Raised by user-provided logic (actions, guards, services).
    #[error("{0}")]
    Logic(String),
    /// JSON (de)serialisation failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Stable classification of this error.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidConfig { .. } => ErrorKind::InvalidConfig,
            Self::RootTarget { .. } => ErrorKind::RootTarget,
            Self::StateNotFound { .. } => ErrorKind::StateNotFound,
            Self::ImplementationMissing { .. } => ErrorKind::ImplementationMissing,
            Self::InvalidEvent(_) | Self::Json(_) => ErrorKind::InvalidEvent,
            Self::UnknownEvent { .. } => ErrorKind::UnknownEvent,
            Self::InvalidLifecycle { .. } => ErrorKind::InvalidLifecycle,
            Self::TransitionFailed { .. } => ErrorKind::TransitionFailed,
            Self::RunawayChain { .. } => ErrorKind::RunawayChain,
            Self::ActorSpawning { .. } => ErrorKind::ActorSpawning,
            Self::UnresolvedTarget { .. } => ErrorKind::UnresolvedTarget,
            Self::LimitExceeded(_) => ErrorKind::LimitExceeded,
            Self::Snapshot(_) => ErrorKind::Snapshot,
            Self::Logic(_) => ErrorKind::Logic,
        }
    }

    pub(crate) fn config(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidConfig { path: path.into(), message: message.into() }
    }

    /// Build a [`Error::Logic`] from any displayable value; the idiomatic way for user logic
    /// to fail.
    pub fn logic(message: impl fmt::Display) -> Self {
        Self::Logic(message.to_string())
    }
}

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
