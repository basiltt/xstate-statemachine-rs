//! XState built-in actions and their JSON-form parameters.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A delay: fixed milliseconds or a name looked up in the logic registry.
#[derive(Debug, Clone, PartialEq)]
pub enum Delay {
    /// Milliseconds.
    Fixed(f64),
    /// Resolved through `MachineLogic::delays` at run time.
    Named(String),
}

impl Delay {
    /// Parse an `after` key or a `delay` param: numbers (or numeric strings) are fixed
    /// milliseconds, anything else is a delay name.
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_f64().map(Self::Fixed),
            Value::String(s) => {
                Some(s.parse::<f64>().map_or_else(|_| Self::Named(s.clone()), Self::Fixed))
            }
            _ => None,
        }
    }
}

/// Recipient of a `sendTo`.
#[derive(Debug, Clone, PartialEq)]
pub enum SendTarget {
    /// The parent actor.
    Parent,
    /// The machine itself (`self`).
    SelfActor,
    /// A child or `systemId` by name.
    Named(String),
}

/// Built-in actions, canonical form.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Builtin {
    /// Merge static values into the context.
    Assign(Map<String, Value>),
    /// Enqueue an event to this machine (internal queue, or delayed).
    Raise {
        /// Event object (`{"type": ...}` or a bare type string).
        event: Value,
        /// Optional delay.
        delay: Option<Delay>,
        /// Optional id for `cancel`.
        id: Option<String>,
    },
    /// Send an event to another actor.
    SendTo {
        /// Recipient.
        to: SendTarget,
        /// Event object.
        event: Value,
        /// Optional delay.
        delay: Option<Delay>,
        /// Optional id for `cancel`.
        id: Option<String>,
    },
    /// Forward the current event to another actor.
    ForwardTo(SendTarget),
    /// Escalate an error to the parent.
    Escalate(Value),
    /// Log a message.
    Log {
        /// Static expression / message (`params.expr`).
        expr: Value,
        /// Optional label.
        label: Option<String>,
    },
    /// Cancel a delayed send by id.
    Cancel(String),
    /// Stop a child actor by id.
    StopChild(String),
    /// Spawn a child actor.
    SpawnChild {
        /// Logic name.
        src: String,
        /// Child id.
        id: Option<String>,
        /// `systemId` registration.
        system_id: Option<String>,
        /// Input value.
        input: Value,
    },
    /// Emit an event to external listeners.
    Emit(Value),
    /// Conditional action batches (`choose`).
    Choose(Vec<ChooseBranch>),
}

/// One `choose` branch (compiled by the machine compiler, which knows how to resolve guards).
#[derive(Debug, Clone)]
pub struct ChooseBranch {
    /// Guard; `None` means "otherwise".
    pub guard: Option<super::Guard>,
    /// Actions when the branch is taken.
    pub actions: Vec<super::ActionDef>,
}

/// Canonical built-in name for an accepted spelling, if any.
#[must_use]
pub(crate) fn canonical(name: &str) -> Option<&'static str> {
    Some(match name {
        "raise" | "raise_" | "xstate.raise" => "raise",
        "sendTo" | "send_to" | "xstate.sendTo" => "sendTo",
        "sendParent" | "send_parent" | "xstate.sendParent" => "sendParent",
        "forwardTo" | "forward_to" | "xstate.forwardTo" => "forwardTo",
        "escalate" | "xstate.escalate" => "escalate",
        "log" | "xstate.log" => "log",
        "cancel" | "xstate.cancel" => "cancel",
        "stopChild" | "stop_child" | "stop" | "xstate.stopChild" | "xstate.stop" => "stopChild",
        "spawnChild" | "spawnChild_" | "xstate.spawnChild" | "xstate.spawn" => "spawnChild",
        "emit" | "xstate.emit" => "emit",
        "assign" | "xstate.assign" => "assign",
        "choose" | "xstate.choose" => "choose",
        _ => return None,
    })
}

fn str_param(params: &Map<String, Value>, key: &str, path: &str) -> Result<Option<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::config(path, format!("`params.{key}` must be a string"))),
    }
}

fn require<'a>(
    params: &'a Map<String, Value>,
    key: &str,
    builtin: &str,
    path: &str,
) -> Result<&'a Value> {
    params
        .get(key)
        .ok_or_else(|| Error::config(path, format!("built-in `{builtin}` requires `params.{key}`")))
}

fn delay_param(params: &Map<String, Value>, path: &str) -> Result<Option<Delay>> {
    match params.get("delay") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Delay::parse(v)
            .map(Some)
            .ok_or_else(|| Error::config(path, "`params.delay` must be a number or a delay name")),
    }
}

fn send_target(value: &Value, path: &str) -> Result<SendTarget> {
    match value.as_str() {
        Some("parent" | "_parent") => Ok(SendTarget::Parent),
        Some("self" | "_self") => Ok(SendTarget::SelfActor),
        Some(name) if !name.is_empty() => Ok(SendTarget::Named(name.to_owned())),
        _ => Err(Error::config(path, "`params.to` must be a non-empty actor name")),
    }
}

/// Compile a built-in action's parameters. `name` must already be canonical.
pub(crate) fn compile_builtin(name: &str, params: &Value, path: &str) -> Result<Builtin> {
    let empty = Map::new();
    let p = match params {
        Value::Null => &empty,
        Value::Object(m) => m,
        _ => {
            return Err(Error::config(
                path,
                format!("`params` of built-in `{name}` must be an object"),
            ));
        }
    };
    Ok(match name {
        "assign" => {
            let assignment = match p.get("assignment") {
                Some(Value::Object(m)) => m.clone(),
                Some(_) => {
                    return Err(Error::config(path, "`params.assignment` must be an object"));
                }
                None => p.clone(),
            };
            Builtin::Assign(assignment)
        }
        "raise" => Builtin::Raise {
            event: require(p, "event", name, path)?.clone(),
            delay: delay_param(p, path)?,
            id: str_param(p, "id", path)?,
        },
        "sendTo" => Builtin::SendTo {
            to: send_target(require(p, "to", name, path)?, path)?,
            event: require(p, "event", name, path)?.clone(),
            delay: delay_param(p, path)?,
            id: str_param(p, "id", path)?,
        },
        "sendParent" => Builtin::SendTo {
            to: SendTarget::Parent,
            event: require(p, "event", name, path)?.clone(),
            delay: delay_param(p, path)?,
            id: str_param(p, "id", path)?,
        },
        "forwardTo" => Builtin::ForwardTo(send_target(require(p, "to", name, path)?, path)?),
        "escalate" => Builtin::Escalate(require(p, "error", name, path)?.clone()),
        "log" => Builtin::Log {
            expr: p.get("expr").cloned().unwrap_or(Value::Null),
            label: str_param(p, "label", path)?,
        },
        "cancel" => Builtin::Cancel(
            str_param(p, "sendId", path)?
                .ok_or_else(|| Error::config(path, "built-in `cancel` requires `params.sendId`"))?,
        ),
        "stopChild" => Builtin::StopChild(
            str_param(p, "id", path)?
                .ok_or_else(|| Error::config(path, "built-in `stopChild` requires `params.id`"))?,
        ),
        "spawnChild" => Builtin::SpawnChild {
            src: str_param(p, "src", path)?.ok_or_else(|| {
                Error::config(path, "built-in `spawnChild` requires `params.src`")
            })?,
            id: str_param(p, "id", path)?,
            system_id: str_param(p, "systemId", path)?,
            input: p.get("input").cloned().unwrap_or(Value::Null),
        },
        "emit" => Builtin::Emit(require(p, "event", name, path)?.clone()),
        "choose" => {
            return Err(Error::config(
                path,
                "internal: `choose` is compiled by the machine compiler",
            ));
        }
        other => return Err(Error::config(path, format!("unknown built-in `{other}`"))),
    })
}
