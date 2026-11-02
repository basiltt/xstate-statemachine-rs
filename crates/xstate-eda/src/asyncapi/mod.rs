//! AsyncAPI 3.0.0 description of a machine (Python `eda/asyncapi.py`).
//!
//! A machine already declares everything AsyncAPI asks for: the events it **consumes** (its
//! `on` keys) and the events it **publishes** (`meta.publish` transitions and `publish`-tagged
//! states, see [`publish_specs`]). [`asyncapi_document`] renders that as an AsyncAPI 3.0.0
//! document whose messages are CloudEvents (structured mode), identical to the Python
//! library's output for the same chart.
//!
//! The AsyncAPI 3.0.0 JSON Schema is vendored ([`load_asyncapi_schema`]);
//! [`validate_asyncapi`] performs a structural check against it (top-level keys, references,
//! operations) rather than full JSON-Schema validation.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};
use xstate_core::Machine;

use crate::chart::walk;
use crate::error::Result;
use crate::outbox::{PublishSource, publish_specs};
use crate::py::repr_str;

mod validate;

pub use validate::{load_asyncapi_schema, validate_asyncapi};

/// The AsyncAPI version generated.
pub const ASYNCAPI_VERSION: &str = "3.0.0";
const CONTENT_TYPE: &str = "application/cloudevents+json";
const ENGINE_PREFIXES: [&str; 4] = ["done.", "error.", "xstate.", "after."];

/// Options of [`asyncapi_document`].
#[derive(Debug, Clone, PartialEq)]
pub struct AsyncApiOptions {
    /// An AsyncAPI Server Object (`{"host", "protocol", ...}`), registered as `default`.
    pub server: Option<Map<String, Value>>,
    /// Channel address of consumed events (default: the machine id).
    pub inbound_topic: Option<String>,
    /// Channel address of published events (the outbox topic; default `events`).
    pub outbound_topic: String,
    /// `info.title` (default `"<machine id> events"`).
    pub title: Option<String>,
}

impl Default for AsyncApiOptions {
    fn default() -> Self {
        Self { server: None, inbound_topic: None, outbound_topic: "events".into(), title: None }
    }
}

/// Every caller-sendable event the chart handles, sorted: wildcards and engine events
/// (`done.*`, `error.*`, `xstate.*`, `after.*`) excluded.
#[must_use]
pub fn consumed_events(machine: &Machine) -> Vec<String> {
    let doc = machine.to_json();
    let mut names = BTreeSet::new();
    for node in walk(&doc, machine.id()) {
        for ev in
            node.config.get("on").and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k)
        {
            if ev.is_empty()
                || ev == "*"
                || ev.ends_with(".*")
                || ENGINE_PREFIXES.iter().any(|p| ev.starts_with(p))
            {
                continue;
            }
            names.insert(ev.clone());
        }
    }
    names.into_iter().collect()
}

/// A component key: characters outside `[A-Za-z0-9_.-]` become `_`.
fn key(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' })
        .collect()
}

/// A component key for `text` not already in `taken` (suffix `_2`, `_3`, ... on collision).
fn unique_key(text: &str, taken: &Map<String, Value>) -> String {
    let base = key(text);
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.contains_key(&candidate) {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    candidate
}

fn cloudevent_schema() -> Value {
    json!({
        "type": "object",
        "required": ["specversion", "id", "source", "type"],
        "properties": {
            "specversion": {"type": "string", "const": "1.0"},
            "id": {"type": "string"},
            "source": {"type": "string"},
            "type": {"type": "string"},
            "subject": {"type": "string", "description": "Machine instance key (partition key)."},
            "time": {"type": "string", "format": "date-time"},
            "datacontenttype": {"type": "string"},
            "data": {},
            "correlationid": {"type": "string"},
            "causationid": {"type": "string"},
            "machineid": {"type": "string"},
            "machineversion": {"type": "string"},
            "traceparent": {"type": "string"},
            "xsmattempt": {"type": "integer", "minimum": 0}
        }
    })
}

fn message(ce_type: &str, summary: &str, fields: Option<&[String]>) -> Value {
    let mut data = Map::new();
    data.insert("type".into(), Value::from("object"));
    if let Some(fields) = fields.filter(|f| !f.is_empty()) {
        data.insert(
            "properties".into(),
            Value::Object(fields.iter().map(|f| (f.clone(), json!({}))).collect()),
        );
    }
    json!({
        "name": ce_type,
        "title": ce_type,
        "summary": summary,
        "contentType": CONTENT_TYPE,
        "payload": {
            "allOf": [
                {"$ref": "#/components/schemas/CloudEvent"},
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": ce_type},
                        "data": Value::Object(data)
                    }
                }
            ]
        }
    })
}

fn reference(path: &str) -> Value {
    json!({ "$ref": path })
}

/// `(components.messages, consumed refs, published refs)`.
type Messages = (Map<String, Value>, Map<String, Value>, Map<String, Value>);

/// Build the messages and the refs of both channels.
fn messages(machine: &Machine, mid: &str) -> Result<Messages> {
    let mut messages = Map::new();
    let mut inbound = Map::new();
    let mut outbound = Map::new();
    for ev in consumed_events(machine) {
        let k = unique_key(&format!("consume.{ev}"), &messages);
        messages.insert(
            k.clone(),
            message(&format!("xsm.{mid}.{ev}"), &format!("Command: send {}.", repr_str(&ev)), None),
        );
        inbound.insert(k.clone(), reference(&format!("#/components/messages/{k}")));
    }
    let mut published = BTreeSet::new();
    for spec in publish_specs(machine)? {
        if !published.insert(spec.ty.clone()) {
            continue; // one message per published type
        }
        let k = unique_key(&format!("publish.{}", spec.ty), &messages);
        let origin = match spec.source {
            PublishSource::Transition => {
                format!(
                    "on {} from {}",
                    repr_str(spec.event.as_deref().unwrap_or_default()),
                    spec.from
                )
            }
            PublishSource::State => format!("on entering {}", spec.from),
        };
        messages.insert(
            k.clone(),
            message(&spec.ty, &format!("Published {origin}."), spec.fields.as_deref()),
        );
        outbound.insert(k.clone(), reference(&format!("#/components/messages/{k}")));
    }
    Ok((messages, inbound, outbound))
}

/// Register one channel and the operation that uses it.
fn add_channel(
    channels: &mut Map<String, Value>,
    operations: &mut Map<String, Value>,
    (channel, address, action, op_id): (&str, &str, &str, String),
    description: &str,
    msgs: Map<String, Value>,
) {
    let op_messages: Vec<Value> =
        msgs.keys().map(|k| reference(&format!("#/channels/{channel}/messages/{k}"))).collect();
    channels.insert(
        channel.to_owned(),
        json!({"address": address, "description": description, "messages": Value::Object(msgs)}),
    );
    operations.insert(
        op_id,
        json!({
            "action": action,
            "channel": reference(&format!("#/channels/{channel}")),
            "messages": op_messages
        }),
    );
}

/// An AsyncAPI 3.0.0 document for `machine`.
///
/// Consumed messages have CloudEvents type `xsm.<machine>.<EVENT>` (what the dispatcher maps
/// back to `EVENT`); published messages carry the type the chart declares.
///
/// # Errors
/// [`EdaError::InvalidSpec`](crate::EdaError::InvalidSpec) for a malformed `meta.publish`.
pub fn asyncapi_document(machine: &Machine, options: &AsyncApiOptions) -> Result<Value> {
    let mid = machine.id();
    let (msgs, inbound, outbound) = messages(machine, mid)?;
    let mut channels = Map::new();
    let mut operations = Map::new();
    if !inbound.is_empty() {
        let address = options.inbound_topic.as_deref().unwrap_or(mid);
        let names = ("inbound", address, "receive", format!("consume_{}", key(mid)));
        add_channel(
            &mut channels,
            &mut operations,
            names,
            &format!("Commands consumed by {mid}."),
            inbound,
        );
    }
    if !outbound.is_empty() {
        let names =
            ("outbound", options.outbound_topic.as_str(), "send", format!("publish_{}", key(mid)));
        add_channel(
            &mut channels,
            &mut operations,
            names,
            &format!("Integration events published by {mid}."),
            outbound,
        );
    }
    let title =
        options.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| format!("{mid} events"));
    let version = machine.version().filter(|v| !v.is_empty()).unwrap_or("0.0.0");
    let mut doc = json!({
        "asyncapi": ASYNCAPI_VERSION,
        "info": {
            "title": title,
            "version": version,
            "description": format!("Generated by xstate-statemachine from machine {}.", repr_str(mid))
        },
        "defaultContentType": CONTENT_TYPE,
        "channels": Value::Object(channels),
        "operations": Value::Object(operations),
        "components": {
            "schemas": {"CloudEvent": cloudevent_schema()},
            "messages": Value::Object(msgs)
        }
    });
    if let (Some(server), Value::Object(map)) =
        (options.server.as_ref().filter(|s| !s.is_empty()), &mut doc)
    {
        map.insert("servers".into(), json!({"default": Value::Object(server.clone())}));
    }
    Ok(doc)
}
