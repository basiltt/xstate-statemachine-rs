//! [`InspectorObserver`]: translates interpreter activity into Stately Inspector messages.
//!
//! Attach one to a [`xstate_core::SyncInterpreter`] (or a tokio actor) and point it at a
//! [`Sink`]. For every machine it reports, in the order the Python `InspectorPlugin` does:
//!
//! 1. `@xstate.actor` when the machine starts (definition + an initial snapshot with
//!    `value: {}`, captured before the initial states are entered);
//! 2. `@xstate.event` `xstate.init` followed by an `@xstate.snapshot`;
//! 3. for each processed event, `@xstate.event` then `@xstate.snapshot`.
//!
//! Dropped events (machine not running, deferred) and errors produce nothing, as in Python.
//!
//! # Data leaving the process
//! Context is deny-by-default: only keys in the context allow-list are sent, and allowed
//! values are run through [`RedactKeys`]. Event payloads are not sent unless
//! [`InspectorObserver::include_payloads`] is set, and then only keys in the payload
//! allow-list (redacted). The machine definition's initial `context` is filtered the same way.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};
use xstate_core::engine::{ActiveState, StepReport};
use xstate_core::snapshot::python_event_name;
use xstate_core::{Context, Event, Machine, Observer};

use crate::protocol::{
    ActorMessage, actor_message, event_message, snapshot_body, snapshot_message, status_of,
};
use crate::sinks::Sink;
use xstate_core::redact::RedactKeys;

/// Source of `createdAt` timestamps: epoch milliseconds.
pub type ClockFn = dyn Fn() -> i64 + Send + Sync;

/// An [`Observer`] that emits Stately Inspector messages to a [`Sink`].
///
/// Build with [`InspectorObserver::new`] and the chained setters, then wrap in an `Arc` and
/// pass to `SyncInterpreter::observe`. One observer serves one interpreter: the session id
/// defaults to the machine id, which is what the Python plugin uses for a top-level
/// interpreter; a persisted instance should set [`InspectorObserver::session_id`] to its
/// store key so several instances of one chart do not share a session.
pub struct InspectorObserver {
    sink: Arc<dyn Sink>,
    context_allowlist: HashSet<String>,
    include_payloads: bool,
    payload_allowlist: HashSet<String>,
    redact_keys: RedactKeys,
    clock: Arc<ClockFn>,
    session_id: Option<String>,
    root_id: Option<String>,
    parent_id: Option<String>,
    name: Option<String>,
    disabled: AtomicBool,
}

impl std::fmt::Debug for InspectorObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectorObserver")
            .field("session_id", &self.session_id)
            .field("include_payloads", &self.include_payloads)
            .field("context_allowlist", &self.context_allowlist)
            .field("disabled", &self.disabled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or_default()
}

impl InspectorObserver {
    /// Report to `sink` with deny-by-default context and payloads and the default denylist.
    pub fn new(sink: impl Sink + 'static) -> Self {
        Self::with_shared_sink(Arc::new(sink))
    }

    /// Like [`InspectorObserver::new`] for a sink shared with other observers.
    #[must_use]
    pub fn with_shared_sink(sink: Arc<dyn Sink>) -> Self {
        Self {
            sink,
            context_allowlist: HashSet::new(),
            include_payloads: false,
            payload_allowlist: HashSet::new(),
            redact_keys: RedactKeys::default(),
            clock: Arc::new(wall_clock_ms),
            session_id: None,
            root_id: None,
            parent_id: None,
            name: None,
            disabled: AtomicBool::new(false),
        }
    }

    /// Context keys that may leave the process (empty: every snapshot carries `context: {}`).
    #[must_use]
    pub fn context_allowlist<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.context_allowlist = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Send event payload fields (redacted, allow-listed). With an empty payload allow-list
    /// events still carry only `type`, and a warning is logged once here.
    #[must_use]
    pub fn include_payloads(mut self, include: bool) -> Self {
        self.include_payloads = include;
        if include && self.payload_allowlist.is_empty() {
            tracing::warn!(
                target: "xstate::inspect",
                "include_payloads(true) without a payload_allowlist sends only the event type; name the payload keys that may leave the process"
            );
        }
        self
    }

    /// Payload keys sent when [`InspectorObserver::include_payloads`] is on (`type` is never
    /// overwritten).
    #[must_use]
    pub fn payload_allowlist<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.payload_allowlist = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the redaction denylist (see [`xstate_core::redact::DEFAULT_REDACT_KEYS`]).
    #[must_use]
    pub fn redact_keys(mut self, keys: RedactKeys) -> Self {
        self.redact_keys = keys;
        self
    }

    /// Inject the `createdAt` clock (epoch milliseconds); tests and replays use a fixed one.
    #[must_use]
    pub fn clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Session id of the reported actor (default: the machine id). Also the root id unless
    /// [`InspectorObserver::root_id`] is set.
    #[must_use]
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Root session id, for a child actor reported under its parent's session.
    #[must_use]
    pub fn root_id(mut self, id: impl Into<String>) -> Self {
        self.root_id = Some(id.into());
        self
    }

    /// Parent session id: sets `parentId` on the actor message and `sourceId` on `xstate.init`.
    #[must_use]
    pub fn parent_id(mut self, id: impl Into<String>) -> Self {
        self.parent_id = Some(id.into());
        self
    }

    /// Display name of the actor (default: the machine id; children use their invoke id).
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// `true` once a sink failure has disabled this observer.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    // -- shaping ---------------------------------------------------------------------------

    fn session<'a>(&'a self, machine: &'a Machine) -> &'a str {
        self.session_id.as_deref().unwrap_or_else(|| machine.id())
    }

    fn root<'a>(&'a self, machine: &'a Machine) -> &'a str {
        self.root_id.as_deref().unwrap_or_else(|| self.session(machine))
    }

    /// The allow-listed, redacted view of a context object (`{}` for anything else).
    fn shape_context(&self, context: &Value) -> Value {
        if self.context_allowlist.is_empty() {
            return Value::Object(Map::new());
        }
        let Value::Object(map) = context else {
            return Value::Object(Map::new());
        };
        let picked: Map<String, Value> = map
            .iter()
            .filter(|(k, _)| self.context_allowlist.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut picked = Value::Object(picked);
        self.redact_keys.redact_in_place(&mut picked);
        picked
    }

    /// The chart config with its initial `context` put through the same allow-list.
    fn definition(&self, machine: &Machine) -> Value {
        let mut config = machine.to_json();
        if let Value::Object(map) = &mut config
            && let Some(ctx) = map.get_mut("context")
        {
            *ctx = self.shape_context(ctx);
        }
        config
    }

    /// `{type, ...allowed payload keys}` for an event.
    fn shape_event(&self, event: &Event) -> Value {
        let mut out = Map::with_capacity(1);
        if event.origin == xstate_core::EventOrigin::Init {
            out.insert("type".into(), "xstate.init".into());
            return Value::Object(out);
        }
        out.insert("type".into(), python_event_name(&event.ty).into());
        if self.include_payloads
            && let Value::Object(payload) = &event.payload
        {
            for (k, v) in payload {
                if k == "type" || !self.payload_allowlist.contains(k.as_str()) {
                    continue;
                }
                let value = if self.redact_keys.matches(k) {
                    Value::from(xstate_core::redact::REDACTED)
                } else {
                    self.redact_keys.redact(v)
                };
                out.insert(k.clone(), value);
            }
        }
        Value::Object(out)
    }

    fn snapshot_of<C: Context>(&self, machine: &Machine, state: &ActiveState<C>) -> Value {
        let context = serde_json::to_value(state.context()).unwrap_or(Value::Null);
        snapshot_body(
            status_of(state.status()),
            state.value(machine),
            self.shape_context(&context),
            &[],
            &state.tags(machine),
        )
    }

    fn emit(&self, message: &Value) {
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = self.sink.send(message) {
            self.disabled.store(true, Ordering::Relaxed);
            tracing::warn!(
                target: "xstate::inspect",
                error = %err,
                "inspector sink failed; inspection disabled for this observer (reported once)"
            );
        }
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn init_event() -> Value {
        let mut ev = Map::with_capacity(1);
        ev.insert("type".into(), "xstate.init".into());
        Value::Object(ev)
    }
}

impl<C: Context> Observer<C> for InspectorObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, _report: &StepReport) {
        let session = self.session(machine);
        let root = Some(self.root(machine));
        let initial = snapshot_body(
            "active",
            Value::Object(Map::new()),
            self.shape_context(machine.initial_context()),
            &[],
            &[],
        );
        let definition = self.definition(machine);
        self.emit(&actor_message(ActorMessage {
            session_id: session,
            name: self.name.as_deref().unwrap_or_else(|| machine.id()),
            root_id: root,
            parent_id: self.parent_id.as_deref(),
            definition: Some(&definition),
            snapshot: initial,
            created_at_ms: self.now(),
        }));
        self.emit(&event_message(
            session,
            Self::init_event(),
            root,
            self.parent_id.as_deref(),
            self.now(),
        ));
        self.emit(&snapshot_message(
            session,
            Self::init_event(),
            self.snapshot_of(machine, state),
            root,
            self.now(),
        ));
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        _report: &StepReport,
        state: &ActiveState<C>,
    ) {
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        let session = self.session(machine);
        let root = Some(self.root(machine));
        let shaped = self.shape_event(event);
        self.emit(&event_message(session, shaped.clone(), root, None, self.now()));
        self.emit(&snapshot_message(
            session,
            shaped,
            self.snapshot_of(machine, state),
            root,
            self.now(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::MemorySink;
    use serde_json::json;

    #[test]
    fn a_non_object_context_is_sent_as_an_empty_object() {
        let observer = InspectorObserver::new(MemorySink::new()).context_allowlist(["a"]);
        for context in [json!(5), json!([1, 2]), json!("text"), Value::Null] {
            assert_eq!(observer.shape_context(&context), json!({}), "{context}");
        }
        assert_eq!(observer.shape_context(&json!({"a": 1, "b": 2})), json!({"a": 1}));
    }
}
