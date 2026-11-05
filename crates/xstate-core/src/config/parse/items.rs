//! Transitions, invokes, actions and guards: the parts of a state node that carry behaviour.

use serde_json::{Map, Value};

use super::{Parser, Path, expect_object, field, within};
use crate::config::{ActionConfig, GuardConfig, InvokeConfig, TransitionConfig};
use crate::error::{Error, Result};
use crate::machine::builtin;

const TRANSITION_KEYS: &[&str] =
    &["target", "actions", "guard", "cond", "internal", "reenter", "meta", "description", "tags"];
const INVOKE_KEYS: &[&str] =
    &["id", "src", "input", "systemId", "onDone", "onError", "meta", "description"];
const ACTION_KEYS: &[&str] = &["type", "params"];
const GUARD_KEYS: &[&str] = &["type", "params", "children"];

/// Where a transition list was written, for error messages.
pub(super) enum Label<'a> {
    /// `on: {descriptor: ...}`.
    On(&'a str),
    /// `after: {key: ...}`.
    After(&'a str),
    /// A fixed key (`always`, `onDone`, `invoke onError`).
    Key(&'a str),
}

/// The state and handler owning a transition list.
pub(super) struct Owner<'a> {
    pub(super) state: &'a str,
    pub(super) label: Label<'a>,
}

impl Owner<'_> {
    /// `State 'm.a' on 'GO'`.
    fn state_label(&self) -> String {
        match self.label {
            Label::On(d) => format!("State '{}' on '{d}'", self.state),
            Label::After(k) => format!("State '{}' after['{k}']", self.state),
            Label::Key(k) => format!("State '{}' {k}", self.state),
        }
    }

    /// `Transition 'GO' on state 'm.a'`.
    fn transition_label(&self) -> String {
        let event = match self.label {
            Label::On(d) => d.to_owned(),
            Label::After(k) => format!("after.{k}"),
            Label::Key(k) => k.to_owned(),
        };
        format!("Transition '{event}' on state '{}'", self.state)
    }
}

/// A transition-level boolean flag (`reenter`, `internal`): `true` / `false`, or `null` for
/// "not set". A string such as `"false"` is refused rather than read as truthy.
fn transition_flag(
    obj: &Map<String, Value>,
    key: &str,
    path: &Path<'_>,
    owner: &Owner<'_>,
) -> Result<Option<bool>> {
    match field(obj, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(Error::config(
            path.key(key).render(),
            format!("{}: `{key}` must be true or false, got {other}", owner.transition_label()),
        )),
    }
}

fn target_string(s: &str, path: &Path<'_>, owner: &Owner<'_>) -> Result<String> {
    if s.is_empty() {
        return Err(Error::config(
            path.render(),
            format!("{}: an empty-string `target` is not a state", owner.transition_label()),
        ));
    }
    Ok(s.to_owned())
}

fn transition_targets(
    value: Option<&Value>,
    path: &Path<'_>,
    owner: &Owner<'_>,
) -> Result<Vec<String>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![target_string(s, path, owner)?]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|t| match t {
                Value::String(s) => target_string(s, path, owner),
                _ => Err(Error::config(path.render(), "targets must be strings")),
            })
            .collect(),
        Some(_) => {
            Err(Error::config(path.render(), "`target` must be a string or array of strings"))
        }
    }
}

impl Parser<'_> {
    pub(super) fn parse_transitions(
        &mut self,
        value: &Value,
        path: &Path<'_>,
        owner: &Owner<'_>,
    ) -> Result<Vec<TransitionConfig>> {
        let single = |this: &mut Self, item: &Value, p: &Path<'_>| -> Result<TransitionConfig> {
            match item {
                Value::String(target) => Ok(TransitionConfig {
                    targets: vec![target_string(target, p, owner)?],
                    ..Default::default()
                }),
                Value::Object(obj) => this.parse_transition(obj, p, owner),
                other => Err(Error::config(
                    p.render(),
                    format!(
                        "{}: invalid transition item {other}; expected a target string or an object",
                        owner.state_label()
                    ),
                )),
            }
        };
        match value {
            Value::Null => {
                Ok(vec![TransitionConfig { forbidden: true, ..TransitionConfig::default() }])
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.push(single(self, item, &path.index(i))?);
                }
                Ok(out)
            }
            Value::String(_) | Value::Object(_) => Ok(vec![single(self, value, path)?]),
            other => Err(Error::config(
                path.render(),
                format!(
                    "{}: transition must be a string, object, array or null, got {other}",
                    owner.state_label()
                ),
            )),
        }
    }

    fn parse_transition(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
        owner: &Owner<'_>,
    ) -> Result<TransitionConfig> {
        self.check_keys(obj, path, TRANSITION_KEYS, &[])?;
        // An empty object is XState's own spelling of a forbidden transition.
        if obj.is_empty() {
            return Ok(TransitionConfig { forbidden: true, ..TransitionConfig::default() });
        }
        let targets = transition_targets(field(obj, "target"), &path.key("target"), owner)?;
        let actions = self
            .parse_actions(field(obj, "actions"), &path.key("actions"))
            .map_err(|e| within(e, || format!("{} actions", owner.transition_label())))?;
        let guard = match (field(obj, "guard"), field(obj, "cond")) {
            (Some(g), _) | (None, Some(g)) => {
                Some(self.parse_guard(g, &path.key("guard")).map_err(|e| {
                    within(e, || format!("{}: invalid 'guard'", owner.transition_label()))
                })?)
            }
            (None, None) => None,
        };
        let reenter = transition_flag(obj, "reenter", path, owner)?;
        let internal = transition_flag(obj, "internal", path, owner)?;
        Ok(TransitionConfig {
            targets,
            actions,
            guard,
            reenter: reenter.or(internal.map(|i| !i)).unwrap_or(false),
            forbidden: false,
            meta: field(obj, "meta").cloned(),
            description: field(obj, "description").and_then(Value::as_str).map(str::to_owned),
        })
    }

    pub(super) fn parse_invoke(
        &mut self,
        value: &Value,
        path: &Path<'_>,
        state: &str,
    ) -> Result<InvokeConfig> {
        let obj = expect_object(value, path, "`invoke`")?;
        self.check_keys(obj, path, INVOKE_KEYS, &[])?;
        let src = match field(obj, "src") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Object(_)) => {
                return Err(Error::config(
                    path.key("src").render(),
                    "inline machines are not supported; name the service",
                ));
            }
            _ => {
                return Err(Error::config(
                    path.key("src").render(),
                    "`src` must be a non-empty string",
                ));
            }
        };
        let id = invoke_name(obj, "id", path, state)?;
        let system_id = invoke_name(obj, "systemId", path, state)?;
        if let Some(d) = field(obj, "description").filter(|d| !d.is_string()) {
            return Err(Error::config(
                path.key("description").render(),
                format!("State '{state}' invoke: 'description' must be a string, got {d}"),
            ));
        }
        let on_done = match field(obj, "onDone") {
            Some(v) => {
                let owner = Owner { state, label: Label::Key("invoke onDone") };
                self.parse_transitions(v, &path.key("onDone"), &owner)?
            }
            None => Vec::new(),
        };
        let on_error = match field(obj, "onError") {
            Some(v) => {
                let owner = Owner { state, label: Label::Key("invoke onError") };
                self.parse_transitions(v, &path.key("onError"), &owner)?
            }
            None => Vec::new(),
        };
        Ok(InvokeConfig {
            id,
            src,
            input: field(obj, "input").cloned().unwrap_or(Value::Null),
            system_id,
            on_done,
            on_error,
        })
    }

    /// An action list: one action, an array of them, or `null`. `0`, `false` and `""` are
    /// refused rather than read as "no actions".
    pub(super) fn parse_actions(
        &mut self,
        value: Option<&Value>,
        path: &Path<'_>,
    ) -> Result<Vec<ActionConfig>> {
        match value {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| self.parse_action(item, &path.index(i)))
                .collect(),
            Some(single) => Ok(vec![self.parse_action(single, path)?]),
        }
    }

    fn parse_action(&mut self, item: &Value, path: &Path<'_>) -> Result<ActionConfig> {
        let obj = match item {
            Value::String(name) if !name.is_empty() => {
                return Ok(ActionConfig { name: name.clone(), params: Value::Null });
            }
            Value::Object(obj) => obj,
            other => {
                return Err(Error::config(
                    path.render(),
                    format!(
                        "expected an action name, an action object or a list of them, got {other}"
                    ),
                ));
            }
        };
        let (name, params) = action_parts(obj, path)?;
        let user = self.user_actions.is_some_and(|names| names.contains(name));
        let canonical = if user { None } else { builtin::canonical(name) };
        if let Some((required, optional)) = canonical.and_then(builtin::param_spec) {
            check_flat_params(obj, name, required, optional, path)?;
            self.check_builtin_params(&params, name, required, optional, path)?;
        }
        for key in obj.keys().filter(|k| !ACTION_KEYS.contains(&k.as_str()) && !k.starts_with("x-"))
        {
            let message = if canonical.is_some() {
                format!(
                    "unexpected key `{key}` on action `{name}`; built-in options go under `params`"
                )
            } else {
                let hint = closest(key, ACTION_KEYS.iter().copied())
                    .map_or_else(String::new, |h| format!(" (did you mean `{h}`?)"));
                format!("unknown key `{key}` on action `{name}`{hint}")
            };
            self.report(&path.key(key), message)?;
        }
        Ok(ActionConfig { name: name.to_owned(), params })
    }

    /// Report the `params` keys a built-in does not read (`"delya"`).
    fn check_builtin_params(
        &mut self,
        params: &Value,
        name: &str,
        required: &[&str],
        optional: &[&str],
        path: &Path<'_>,
    ) -> Result<()> {
        let Value::Object(p) = params else { return Ok(()) };
        let known = |k: &str| required.contains(&k) || optional.contains(&k);
        for key in p.keys().filter(|k| !known(k)) {
            let hint = closest(key, required.iter().chain(optional).copied())
                .map_or_else(String::new, |h| format!(" (did you mean `{h}`?)"));
            let message = format!("unknown param `{key}` of built-in `{name}`{hint}");
            self.report(&path.key("params").key(key), message)?;
        }
        Ok(())
    }

    pub(super) fn parse_guard(&mut self, value: &Value, path: &Path<'_>) -> Result<GuardConfig> {
        let obj = match value {
            Value::String(s) => return named_guard(s, path),
            Value::Object(obj) => obj,
            _ => return Err(Error::config(path.render(), "guard must be a string or an object")),
        };
        let ty = field(obj, "type").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(
            || Error::config(path.render(), "guard object needs a non-empty string `type`"),
        )?;
        for key in obj.keys().filter(|k| !GUARD_KEYS.contains(&k.as_str()) && !k.starts_with("x-"))
        {
            let hint = closest(key, GUARD_KEYS.iter().copied())
                .map_or_else(String::new, |h| format!(" (did you mean `{h}`?)"));
            self.report(&path.key(key), format!("unknown key `{key}` on guard `{ty}`{hint}"))?;
        }
        let params = guard_params(obj, ty, path)?;
        match ty {
            "and" | "or" => {
                let operands = self.guard_operands(obj, &params, ty, path)?;
                Ok(if ty == "and" { GuardConfig::And(operands) } else { GuardConfig::Or(operands) })
            }
            "not" => {
                let inner = params
                    .get("guard")
                    .or_else(|| {
                        field(obj, "children").and_then(|c| c.as_array()).and_then(|a| a.first())
                    })
                    .ok_or_else(|| {
                        Error::config(
                            path.render(),
                            "`not` guard needs `params.guard` or one child",
                        )
                    })?;
                Ok(GuardConfig::Not(Box::new(self.parse_guard(inner, &path.key("guard"))?)))
            }
            _ => Ok(GuardConfig::Named { name: ty.to_owned(), params }),
        }
    }

    fn guard_operands(
        &mut self,
        obj: &Map<String, Value>,
        params: &Value,
        ty: &str,
        path: &Path<'_>,
    ) -> Result<Vec<GuardConfig>> {
        let list = obj
            .get("children")
            .or_else(|| params.get("guards"))
            .or_else(|| params.get("children"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Error::config(
                    path.render(),
                    format!("`{ty}` guard needs operands under `children` or `params.guards`"),
                )
            })?;
        list.iter().enumerate().map(|(i, g)| self.parse_guard(g, &path.index(i))).collect()
    }
}

/// A guard object's `params`: an object or `null` (`stateIn` also takes a bare state id).
fn guard_params(obj: &Map<String, Value>, ty: &str, path: &Path<'_>) -> Result<Value> {
    match field(obj, "params") {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(p @ Value::Object(_)) => Ok(p.clone()),
        Some(p @ Value::String(_)) if ty == "stateIn" => Ok(p.clone()),
        Some(other) => Err(Error::config(
            path.key("params").render(),
            format!("guard '{ty}': 'params' must be an object, got {other}"),
        )),
    }
}

fn named_guard(s: &str, path: &Path<'_>) -> Result<GuardConfig> {
    if let Some(inner) = s.strip_prefix('!') {
        if inner.is_empty() {
            return Err(Error::config(path.render(), "`!` must be followed by a guard name"));
        }
        return Ok(GuardConfig::Not(Box::new(GuardConfig::Named {
            name: inner.to_owned(),
            params: Value::Null,
        })));
    }
    if s.is_empty() {
        return Err(Error::config(path.render(), "guard name must be non-empty"));
    }
    Ok(GuardConfig::Named { name: s.to_owned(), params: Value::Null })
}

/// An invoke `id` / `systemId`: absent, or a non-empty string.
fn invoke_name(
    obj: &Map<String, Value>,
    key: &str,
    path: &Path<'_>,
    state: &str,
) -> Result<Option<String>> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(other) => Err(Error::config(
            path.key(key).render(),
            format!("State '{state}' invoke: '{key}' must be a non-empty string, got {other}"),
        )),
    }
}

/// The `type` and `params` of an action object: a non-empty string and an object (or `null`).
fn action_parts<'v>(obj: &'v Map<String, Value>, path: &Path<'_>) -> Result<(&'v str, Value)> {
    let name = match field(obj, "type") {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        other => {
            let got = other.map_or_else(|| "nothing".to_owned(), ToString::to_string);
            return Err(Error::config(
                path.render(),
                format!("action object needs a non-empty string `type`, got {got}"),
            ));
        }
    };
    let params = match field(obj, "params") {
        None | Some(Value::Null) => Value::Null,
        Some(p @ Value::Object(_)) => p.clone(),
        Some(other) => {
            return Err(Error::config(
                path.key("params").render(),
                format!("Action '{name}': 'params' must be an object, got {other}"),
            ));
        }
    };
    Ok((name, params))
}

/// A built-in written flat (`{"type": "raise", "event": "X"}`): its required params were put
/// next to `type` instead of under `params`. Refused with a hint; it would otherwise compile
/// to a built-in missing its parameters.
fn check_flat_params(
    obj: &Map<String, Value>,
    name: &str,
    required: &[&str],
    optional: &[&str],
    path: &Path<'_>,
) -> Result<()> {
    let params = field(obj, "params").and_then(Value::as_object);
    let missing = required.iter().any(|k| !params.is_some_and(|p| p.contains_key(*k)));
    let mut stray: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| required.contains(k) || optional.contains(k))
        .collect();
    if !missing || stray.is_empty() {
        return Ok(());
    }
    stray.sort_unstable();
    Err(Error::config(
        path.render(),
        format!(
            "built-in action `{name}` is missing required params; found {stray:?} at the top \
             level of the action -- built-in action parameters must be nested under `params`"
        ),
    ))
}

/// Parse a guard written as a detached JSON value (used by the `choose` compiler). Unknown keys
/// are reported in `warnings` (an error under `strict`).
pub(crate) fn parse_guard_at(
    value: &Value,
    path: &str,
    strict: bool,
    warnings: &mut Vec<crate::config::ConfigWarning>,
) -> Result<GuardConfig> {
    let mut parser = Parser { strict_config: strict, warnings: Vec::new(), user_actions: None };
    let root = Path::ROOT;
    let out = parser.parse_guard(value, &root.key(path));
    warnings.append(&mut parser.warnings);
    out
}

/// Parse an action list written as a detached JSON value (used by the `choose` compiler).
pub(crate) fn parse_actions_at(
    value: Option<&Value>,
    path: &str,
    strict: bool,
    warnings: &mut Vec<crate::config::ConfigWarning>,
) -> Result<Vec<ActionConfig>> {
    let mut parser = Parser { strict_config: strict, warnings: Vec::new(), user_actions: None };
    let root = Path::ROOT;
    let out = parser.parse_actions(value, &root.key(path));
    warnings.append(&mut parser.warnings);
    out
}

pub(super) fn parse_tags(value: Option<&Value>, path: &Path<'_>) -> Result<Vec<String>> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|t| {
                t.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::config(path.render(), "tags must be strings"))
            })
            .collect(),
        Some(_) => Err(Error::config(path.render(), "`tags` must be a string or array of strings")),
    }
}

/// Closest known key by a cheap edit-distance, for "did you mean" hints.
pub(super) fn closest<'a>(key: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let lower = key.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let d = levenshtein(&lower, &c.to_ascii_lowercase());
        if d <= 2 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}
