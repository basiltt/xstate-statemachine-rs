//! Event-driven architecture for `xstate-core` machines, ported from the Python library's
//! `xstate_statemachine.eda` package (plus `patterns.dead_letter`):
//!
//! * [`Envelope`]: the CloudEvents 1.0 wire format, with sortable ids ([`new_id`]) and
//!   validation; byte-identical JSON to the Python library;
//! * [`BrokerAdapter`] / [`SyncBrokerAdapter`] and [`Delivery`]: the one broker contract;
//!   [`FakeBrokerAdapter`] / [`SyncFakeBrokerAdapter`] for tests;
//! * [`InboundDispatcher`]: envelope → instance → event, with inbox dedup, per-subject order,
//!   bounded retries and poison → dead letters (X0.8); [`replay_dead_letter`];
//! * the transactional outbox: [`OutboxPlugin`], [`OutboxStore`] ([`MemoryOutboxStore`],
//!   [`SqliteOutboxStore`]) and [`OutboxRelay`] with claim leases;
//! * dead letters: [`DeadLetter`], [`DeadLetterPlugin`], [`MemoryDeadLetterStore`],
//!   [`SqliteDeadLetterStore`], [`BrokerDeadLetterSink`];
//! * [`asyncapi_document`]: AsyncAPI 3.0 from a chart.
//!
//! SQLite tables (`xsm_outbox`, `xsm_dead_letters`, `xsm_dlq_audit`) and every JSON shape are
//! the Python library's, so a database or a broker can be shared between the two runtimes.
//! The crate is runtime-agnostic: the async broker contract uses return-position
//! `impl Future + Send`, and the dispatcher calls a caller-supplied [`EnvelopeHandler`]
//! instead of owning an interpreter.

#![forbid(unsafe_code)]

pub mod asyncapi;
pub mod broker;
mod chart;
pub mod dead_letter;
pub mod dispatcher;
pub mod envelope;
mod error;
pub mod fake;
pub mod outbox;
mod py;
#[cfg(feature = "sqlite")]
mod sqlite_util;

pub use asyncapi::{
    ASYNCAPI_VERSION, AsyncApiOptions, asyncapi_document, consumed_events, load_asyncapi_schema,
    validate_asyncapi,
};
pub use broker::{BrokerAdapter, Delivery, Subscription, SyncBrokerAdapter};
#[cfg(feature = "sqlite")]
pub use dead_letter::{AuditEntry, SqliteDeadLetterStore};
pub use dead_letter::{
    BrokerDeadLetterSink, DEAD_LETTER_TAG, DeadLetter, DeadLetterPlugin, DeadLetterSink,
    DeadLetterStore, ERRORS_CONTEXT_KEY, ErrorEntry, MemoryDeadLetterStore, ReplayOptions,
    ReplayResult, dlq_topic, redact_record, replay_dead_letter, sink_fn,
};
pub use dispatcher::{
    DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_BATCH, DEFAULT_MAX_IN_FLIGHT, Dispatch, DispatchResult,
    EnvelopeHandler, HandlerError, InboundDispatcher, OnUnknown, machines_by_type,
};
pub use envelope::{
    ATTEMPT_EXTENSION, Envelope, EnvelopeBuilder, MAX_DATA_DEPTH, SPECVERSION, TransitionOrigin,
    default_event_name, new_id,
};
pub use error::{BrokerError, EdaError, EnvelopeError, Result};
pub use fake::{DEFAULT_DRAIN_LIMIT, FakeBrokerAdapter, SyncFakeBrokerAdapter};
#[cfg(feature = "sqlite")]
pub use outbox::SqliteOutboxStore;
pub use outbox::{
    DEFAULT_CLAIM_LEASE_S, MemoryOutboxStore, OutboxPlugin, OutboxRecord, OutboxRelay, OutboxStore,
    PUBLISH_TAG, PublishSource, PublishSpec, publish_specs,
};
pub use py::{WallClock, now_iso, wall_now};
