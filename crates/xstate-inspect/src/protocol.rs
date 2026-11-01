//! Stately Inspector wire protocol (`@statelyai/inspect` 0.7.2): message builders.
//!
//! Three message kinds, each carrying `type`, `sessionId`, `_version`, `createdAt` (epoch
//! milliseconds as a string), `id: null` and `rootId`:
//!
//! * `@xstate.actor` — an actor was created: `name`, optional `parentId`, `definition` (the
//!   machine config as a JSON *string*), `snapshot`;
//! * `@xstate.event` — an event was delivered: `event`, optional `sourceId`;
//! * `@xstate.snapshot` — the state after processing: `event`, `snapshot`.
//!
//! Key order and spelling follow the Python library's `inspect/protocol.py`, which was pinned
//! against recordings of the real npm package, so a recording from either runtime is
//! interchangeable. Keys whose value would be `undefined` in JavaScript are omitted.

use serde_json::{Map, Value};
use xstate_core::Status;

/// The `@statelyai/inspect` version the message shapes were recorded from.
pub const PROTOCOL_VERSION: &str = "0.7.2";

/// Message `type` values.
pub const MESSAGE_TYPES: [&str; 3] = ["@xstate.actor", "@xstate.event", "@xstate.snapshot"];

/// `true` when `kind` is one of [`MESSAGE_TYPES`].
#[must_use]
pub fn is_message_type(kind: &str) -> bool {
    MESSAGE_TYPES.contains(&kind)
}

/// Engine status → XState snapshot status (`running` is reported as `active`).
#[must_use]
pub const fn status_of(status: Status) -> &'static str {
    match status {
        Status::Running => "active",
        Status::Done => "done",
        Status::Error => "error",
        Status::Stopped => "stopped",
    }
}

/// Common header of every message. `created_at_ms` is rendered as a decimal string.
fn base(
    kind: &str,
    session_id: &str,
    root_id: Option<&str>,
    created_at_ms: i64,
) -> Map<String, Value> {
    let mut msg = Map::with_capacity(10);
    msg.insert("type".into(), kind.into());
    msg.insert("sessionId".into(), session_id.into());
    msg.insert("_version".into(), PROTOCOL_VERSION.into());
    msg.insert("createdAt".into(), created_at_ms.to_string().into());
    msg.insert("id".into(), Value::Null);
    if let Some(root) = root_id {
        msg.insert("rootId".into(), root.into());
    }
    msg
}

/// The `snapshot` object shared by actor and snapshot messages.
///
/// `children` are rendered as `{id: {"id": id}}` sorted by id; `tags` are sorted;
/// `historyValue` is always `{}` (the Inspector does not use it).
#[must_use]
pub fn snapshot_body(
    status: &str,
    value: Value,
    context: Value,
    children: &[String],
    tags: &[String],
) -> Value {
    let mut sorted_children: Vec<&String> = children.iter().collect();
    sorted_children.sort();
    sorted_children.dedup();
    let children_obj: Map<String, Value> = sorted_children
        .into_iter()
        .map(|c| {
            let mut entry = Map::with_capacity(1);
            entry.insert("id".into(), c.as_str().into());
            (c.clone(), Value::Object(entry))
        })
        .collect();
    let mut sorted_tags: Vec<&String> = tags.iter().collect();
    sorted_tags.sort();
    sorted_tags.dedup();

    let mut body = Map::with_capacity(6);
    body.insert("status".into(), status.into());
    body.insert("value".into(), value);
    body.insert("context".into(), context);
    body.insert("children".into(), Value::Object(children_obj));
    body.insert("historyValue".into(), Value::Object(Map::new()));
    body.insert("tags".into(), sorted_tags.into_iter().map(|t| Value::from(t.as_str())).collect());
    Value::Object(body)
}

/// Inputs of an `@xstate.actor` message.
#[derive(Debug, Clone)]
pub struct ActorMessage<'a> {
    /// Receiving session (the actor's id).
    pub session_id: &'a str,
    /// Display name (the machine id, or the invoke id for children).
    pub name: &'a str,
    /// Root session id.
    pub root_id: Option<&'a str>,
    /// Parent session id for child actors.
    pub parent_id: Option<&'a str>,
    /// Machine configuration; `None` renders `{"id": name}`.
    pub definition: Option<&'a Value>,
    /// Initial snapshot body (see [`snapshot_body`]).
    pub snapshot: Value,
    /// Epoch milliseconds.
    pub created_at_ms: i64,
}

/// Build an `@xstate.actor` message. `definition` is serialised the way Python's
/// `json.dumps(config, sort_keys=True)` does (sorted keys, default separators, ASCII).
#[must_use]
pub fn actor_message(input: ActorMessage<'_>) -> Value {
    let mut msg = base("@xstate.actor", input.session_id, input.root_id, input.created_at_ms);
    msg.insert("name".into(), input.name.into());
    if let Some(parent) = input.parent_id {
        msg.insert("parentId".into(), parent.into());
    }
    let definition = if let Some(def) = input.definition {
        xstate_core::pyjson::dumps(def)
    } else {
        let mut fallback = Map::with_capacity(1);
        fallback.insert("id".into(), input.name.into());
        xstate_core::pyjson::dumps(&Value::Object(fallback))
    };
    msg.insert("definition".into(), definition.into());
    msg.insert("snapshot".into(), input.snapshot);
    Value::Object(msg)
}

/// Build an `@xstate.event` message: `event` was delivered to `session_id`, optionally from
/// `source_id`.
#[must_use]
pub fn event_message(
    session_id: &str,
    event: Value,
    root_id: Option<&str>,
    source_id: Option<&str>,
    created_at_ms: i64,
) -> Value {
    let mut msg = base("@xstate.event", session_id, root_id, created_at_ms);
    msg.insert("event".into(), event);
    if let Some(source) = source_id {
        msg.insert("sourceId".into(), source.into());
    }
    Value::Object(msg)
}

/// Build an `@xstate.snapshot` message: the actor's state after processing `event`.
#[must_use]
pub fn snapshot_message(
    session_id: &str,
    event: Value,
    snapshot: Value,
    root_id: Option<&str>,
    created_at_ms: i64,
) -> Value {
    let mut msg = base("@xstate.snapshot", session_id, root_id, created_at_ms);
    msg.insert("event".into(), event);
    msg.insert("snapshot".into(), snapshot);
    Value::Object(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xstate_core::pyjson::dumps_compact;

    #[test]
    fn header_key_order_matches_python() {
        let msg = event_message(
            "s",
            serde_json::json!({"type": "GO"}),
            Some("s"),
            None,
            1_700_000_000_000,
        );
        assert_eq!(
            dumps_compact(&msg),
            r#"{"type":"@xstate.event","sessionId":"s","_version":"0.7.2","createdAt":"1700000000000","id":null,"rootId":"s","event":{"type":"GO"}}"#
        );
    }

    #[test]
    fn optional_keys_are_omitted() {
        let msg = event_message("s", serde_json::json!({"type": "GO"}), None, Some("p"), 0);
        let obj = msg.as_object().unwrap_or_else(|| unreachable!());
        assert!(!obj.contains_key("rootId"));
        assert_eq!(obj["sourceId"], "p");
    }

    #[test]
    fn snapshot_body_sorts_children_and_tags() {
        let body = snapshot_body(
            "active",
            "a".into(),
            Value::Object(Map::new()),
            &["b".into(), "a".into(), "a".into()],
            &["z".into(), "y".into()],
        );
        assert_eq!(
            dumps_compact(&body),
            r#"{"status":"active","value":"a","context":{},"children":{"a":{"id":"a"},"b":{"id":"b"}},"historyValue":{},"tags":["y","z"]}"#
        );
    }

    #[test]
    fn actor_definition_falls_back_to_id() {
        let msg = actor_message(ActorMessage {
            session_id: "s",
            name: "m",
            root_id: Some("s"),
            parent_id: Some("p"),
            definition: None,
            snapshot: Value::Null,
            created_at_ms: 5,
        });
        assert_eq!(msg["definition"], r#"{"id": "m"}"#);
        assert_eq!(msg["parentId"], "p");
    }

    #[test]
    fn status_mapping() {
        assert_eq!(status_of(Status::Running), "active");
        assert_eq!(status_of(Status::Done), "done");
        assert!(is_message_type("@xstate.actor"));
        assert!(!is_message_type("@xstate.other"));
    }
}
