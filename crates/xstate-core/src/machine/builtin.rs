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
    /// Parse a `delay` param: a number, or a numeric string (`"500"`, `"1e3"`), is fixed
    /// milliseconds; any other string is a delay name. Anything else (`true`, an object) is
    /// refused.
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_f64().filter(|ms| ms.is_finite()).map(Self::Fixed),
            Value::String(s) => Some(
                numeric_ms(s)
                    .filter(|ms| ms.is_finite())
                    .map_or_else(|| Self::Named(s.clone()), Self::Fixed),
            ),
            _ => None,
        }
    }

    /// Parse an `after` key: whole, non-negative milliseconds (an integer or a numeric string
    /// such as `" 100"` or `"1e3"`) or a delay name. Returns the delay and its normalised key
    /// (`"1000"` for `"1e3"`), which names the timer event and enters the structure hash, as
    /// in XState (`xstate.after.1000.<id>`) and the Python library.
    ///
    /// # Errors
    /// The reason, for a fractional, negative or non-finite number and an empty name.
    pub fn parse_after_key(key: &str) -> std::result::Result<(Self, String), String> {
        let Some(ms) = numeric_ms(key) else {
            if key.trim().is_empty() {
                return Err("an empty delay name".to_owned());
            }
            return Ok((Self::Named(key.to_owned()), key.to_owned()));
        };
        if !ms.is_finite() || ms < 0.0 || ms.fract() != 0.0 || ms >= 9.0e15 {
            return Err(format!("{ms} is not a whole, non-negative number of milliseconds"));
        }
        #[allow(clippy::cast_possible_truncation)]
        let whole = ms as i64;
        Ok((Self::Fixed(ms), whole.to_string()))
    }
}

/// The value of a numeric-looking string, in the grammar the Python library accepts for delay
/// keys (`[+-]?(digits[.digits]|.digits)([eE][+-]?digits)?`, surrounding whitespace allowed):
/// `"inf"` and `"NaN"` are names, not numbers.
fn numeric_ms(s: &str) -> Option<f64> {
    let t = s.trim();
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) && (frac.is_empty() || digits(frac))) || (int.is_empty() && digits(frac))
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if mantissa_ok && exponent_ok { t.parse::<f64>().ok() } else { None }
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

/// Parameter keys a built-in reads: `(required, optional)`. `assign` and `choose` are
/// validated by their own compilers and are absent.
pub(crate) fn param_spec(
    canonical: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match canonical {
        "raise" | "sendParent" => (&["event"], &["delay", "id"]),
        "sendTo" => (&["to", "event"], &["delay", "id"]),
        "forwardTo" => (&["to"], &[]),
        "escalate" => (&["error"], &[]),
        "cancel" => (&["sendId"], &[]),
        "stopChild" => (&["id"], &[]),
        "spawnChild" => (&["src"], &["id", "systemId", "input"]),
        "emit" => (&["event"], &[]),
        "log" => (&[], &["expr", "label"]),
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
