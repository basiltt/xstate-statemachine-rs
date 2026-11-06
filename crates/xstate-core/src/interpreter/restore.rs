//! Snapshots of a [`SyncInterpreter`] and restoring one from them.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{SyncInterpreter, Timer};
use crate::clock::Clock;
use crate::context::Context;
use crate::engine::ActiveState;
use crate::error::{Error, Result};
use crate::event::{Event, EventOrigin};
use crate::logic::MachineLogic;
use crate::machine::Machine;
use crate::snapshot::{self, Deadline, ScheduledSend, Snapshot};

/// How `after` timers found in a snapshot are re-armed on restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartTimers {
    /// Leave them parked (nothing fires).
    Parked,
    /// Re-arm with the declared delay from now.
    Restart,
    /// Re-arm with the remaining time (`due_at_wall - now`), clamped to the declared delay.
    #[default]
    Resume,
    /// Like `Resume`, and fire everything already due before `from_snapshot` returns, so a
    /// scanner that woke the instance sees the timer's effects immediately.
    FireDue,
}

/// Options for [`SyncInterpreter::from_snapshot`].
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Refuse a snapshot whose `machine_hash` differs from this machine's (default `true`).
    pub verify_hash: Option<bool>,
    /// Timer policy.
    pub restart_timers: RestartTimers,
}

impl<C: Context> SyncInterpreter<C> {
    /// Capture the runtime state in the Python-compatible layout v4.
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] before `start`.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let state = self.state.as_ref().ok_or_else(|| Error::InvalidLifecycle {
            status: "uninitialized",
            message: "snapshot before start".into(),
        })?;
        let machine = &self.machine;
        let now_ms = self.clock.now_ms();
        let wall = self.clock.wall_now();
        let mut deadlines = Vec::new();
        let mut scheduled_sends = Vec::new();
        let mut live: Vec<&Timer> =
            self.timers.iter().filter(|t| !self.cancelled.contains(&t.seq)).collect();
        live.sort_by(|a, b| a.due_ms.total_cmp(&b.due_ms).then(a.seq.cmp(&b.seq)));
        for t in live {
            let remaining = (t.due_ms - now_ms).max(0.0);
            match (t.owner, t.entry_seq) {
                (Some(owner), Some(entry_seq)) => deadlines.push(Deadline {
                    state_id: machine.node(owner).id.to_string(),
                    entry_seq,
                    due_at_wall: wall + remaining / 1000.0,
                    delay_ms: json_number(t.delay_ms),
                    event_type: snapshot::python_event_name(&t.event.ty),
                }),
                _ => scheduled_sends.push(ScheduledSend {
                    kind: "event".into(),
                    ty: t.event.ty.to_string(),
                    payload: if t.event.payload.is_null() {
                        Value::Object(Map::new())
                    } else {
                        t.event.payload.clone()
                    },
                    remaining_ms: remaining,
                    send_id: t.id.clone(),
                }),
            }
        }
        let mut snap = Snapshot::from_state(machine, state)?;
        snap.taken_at = wall;
        snap.deferred = self.deferred.iter().map(event_record).collect();
        snap.pending_events = self.queue.iter().map(event_record).collect();
        snap.scheduled_sends = scheduled_sends;
        snap.deadlines = deadlines;
        Ok(snap)
    }

    /// [`snapshot`](Self::snapshot) serialised to JSON text.
    ///
    /// # Errors
    /// See [`snapshot`](Self::snapshot).
    pub fn snapshot_json(&self) -> Result<String> {
        self.snapshot()?.to_json()
    }

    /// Rebuild an interpreter from a snapshot. Entry actions, invokes and timers do **not**
    /// re-run; timers follow `options.restart_timers`.
    ///
    /// # Errors
    /// [`Error::Snapshot`] when the snapshot belongs to another machine, its hash differs
    /// (unless `verify_hash` is `false`), or its configuration is illegal for this machine.
    pub fn from_snapshot(
        machine: Machine,
        logic: MachineLogic<C>,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock>,
        options: &RestoreOptions,
    ) -> Result<Self> {
        if snapshot.machine_id != machine.id() {
            return Err(Error::Snapshot(format!(
                "snapshot belongs to machine '{}', not '{}'",
                snapshot.machine_id,
                machine.id()
            )));
        }
        if options.verify_hash.unwrap_or(true) && snapshot.machine_hash != machine.structure_hash()
        {
            return Err(Error::Snapshot(format!(
                "machine structure changed since the snapshot was taken (hash {} vs {})",
                snapshot.machine_hash,
                machine.structure_hash()
            )));
        }
        let configuration = snapshot::resolve_configuration(&machine, &snapshot.configuration)?;
        let mut history = HashMap::new();
        for (parent, ids) in &snapshot.history {
            let p = machine.state_by_id(parent).ok_or_else(|| Error::StateNotFound {
                target: parent.clone(),
                from: "snapshot.history".into(),
            })?;
            let mut states = Vec::with_capacity(ids.len());
            for id in ids {
                states.push(machine.state_by_id(id).ok_or_else(|| Error::StateNotFound {
                    target: id.clone(),
                    from: "snapshot.history".into(),
                })?);
            }
            states.sort();
            history.insert(p, states);
        }
        let context: C = serde_json::from_value(snapshot.context.clone())
            .map_err(|e| Error::Snapshot(format!("context does not fit the context type: {e}")))?;
        let state = ActiveState::restore(&machine, &configuration, context, history, snapshot);

        let mut interp = Self::with_clock(machine, logic, clock);
        interp.logic.validate(&interp.machine)?;
        interp.state = Some(state);
        for ev in &snapshot.pending_events {
            interp.queue.push_back(event_from_record(ev)?);
        }
        for ev in &snapshot.deferred {
            interp.deferred.push_back(event_from_record(ev)?);
        }
        let now_wall = interp.clock.wall_now();
        for d in &snapshot.deadlines {
            let Some(owner) = interp.machine.state_by_id(&d.state_id) else {
                return Err(Error::StateNotFound {
                    target: d.state_id.clone(),
                    from: "snapshot.deadlines".into(),
                });
            };
            // Only a state with `after` entries owns timers: the engine cancels a state's
            // timers on exit only for such a state.
            if interp.machine.node(owner).after.is_empty() {
                return Err(Error::Snapshot(format!(
                    "deadline for state '{}', which declares no `after` timer",
                    d.state_id
                )));
            }
            let delay_ms = d.delay_ms.as_f64().unwrap_or(0.0);
            let remaining = match options.restart_timers {
                RestartTimers::Parked => continue,
                RestartTimers::Restart => delay_ms,
                RestartTimers::Resume | RestartTimers::FireDue => {
                    ((d.due_at_wall - now_wall) * 1000.0).clamp(0.0, delay_ms)
                }
            };
            let event = Event::engine(
                snapshot::engine_event_name(&d.event_type),
                Value::Null,
                EventOrigin::After,
            );
            interp.arm_with(remaining, delay_ms, event, Some(owner), None, Some(d.entry_seq));
        }
        for s in &snapshot.scheduled_sends {
            let payload = if s.payload.as_object().is_some_and(Map::is_empty) {
                Value::Null
            } else {
                s.payload.clone()
            };
            let event = Event::engine(s.ty.clone(), payload, EventOrigin::Internal);
            interp.arm_with(s.remaining_ms, s.remaining_ms, event, None, s.send_id.clone(), None);
        }
        if options.restart_timers == RestartTimers::FireDue {
            interp.tick()?;
        }
        Ok(interp)
    }
}

fn json_number(ms: f64) -> Value {
    if ms.fract() == 0.0 && ms.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        Value::from(ms as i64)
    } else {
        Value::from(ms)
    }
}

fn event_record(event: &Event) -> Value {
    let mut map = Map::new();
    map.insert("kind".into(), Value::String("event".into()));
    map.insert("type".into(), Value::String(event.ty.to_string()));
    if !event.payload.is_null() {
        map.insert("payload".into(), event.payload.clone());
    }
    Value::Object(map)
}

/// Every `kind` an event record may carry (the Python `persist_event` vocabulary).
const EVENT_KINDS: [&str; 5] = ["event", "system", "done", "error", "after"];

/// Rebuild a pending or deferred event. A record that is not an object, has no non-empty
/// `type`, carries an unknown `kind`, a non-object `payload` or a non-numeric timer timestamp
/// is a corrupt snapshot ([`Error::Snapshot`]), never user traffic.
fn event_from_record(record: &Value) -> Result<Event> {
    let corrupt = |why: String| Error::Snapshot(format!("snapshot is malformed: {why}"));
    let obj = record
        .as_object()
        .ok_or_else(|| corrupt(format!("event record is {record}, expected an object")))?;
    let ty =
        obj.get("type").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| {
            corrupt(format!("event record {record} has no non-empty string `type`"))
        })?;
    let kind = match obj.get("kind") {
        None | Some(Value::Null) => "event",
        Some(Value::String(k)) if EVENT_KINDS.contains(&k.as_str()) => k.as_str(),
        Some(other) => {
            return Err(corrupt(format!(
                "event record '{ty}' has unknown `kind` {other}; expected one of {EVENT_KINDS:?}"
            )));
        }
    };
    let ty = snapshot::engine_event_name(ty);
    let (payload, origin) = record_payload(obj, kind, &ty)?;
    Ok(Event::engine(ty, payload, origin.unwrap_or(EventOrigin::External)))
}

/// Payload and provenance of one record. Engine kinds restore as engine events only when the
/// record carries `"engine": true`; without it they are user traffic.
fn record_payload(
    obj: &Map<String, Value>,
    kind: &str,
    ty: &str,
) -> Result<(Value, Option<EventOrigin>)> {
    let corrupt = |why: String| Error::Snapshot(format!("snapshot is malformed: {why}"));
    let engine = obj.get("engine") == Some(&Value::Bool(true));
    let wrapped = |key: &str, value: Option<&Value>| {
        let mut map = Map::with_capacity(1);
        map.insert(key.to_owned(), value.cloned().unwrap_or(Value::Null));
        Value::Object(map)
    };
    Ok(match kind {
        "done" => {
            let origin = if ty.starts_with("xstate.done.state.") {
                EventOrigin::DoneState
            } else {
                EventOrigin::DoneActor
            };
            (wrapped("output", obj.get("data")), engine.then_some(origin))
        }
        "error" => (wrapped("error", obj.get("error")), engine.then_some(EventOrigin::ErrorActor)),
        "after" => {
            for key in ["scheduled_for", "fired_at"] {
                if obj.get(key).is_some_and(|v| !v.is_null() && !v.is_number()) {
                    return Err(corrupt(format!("event record '{ty}' has a non-numeric `{key}`")));
                }
            }
            (Value::Null, engine.then_some(EventOrigin::After))
        }
        _ => {
            let payload = match obj.get("payload") {
                None | Some(Value::Null) => Value::Null,
                Some(p @ Value::Object(_)) => p.clone(),
                Some(other) => {
                    return Err(corrupt(format!(
                        "event record '{ty}' has a non-object `payload` {other}"
                    )));
                }
            };
            (payload, (kind == "system").then_some(EventOrigin::Internal))
        }
    })
}
