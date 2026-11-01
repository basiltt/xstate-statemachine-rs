//! Lenient shape parser for XState v5 JSON.
//!
//! Diagnostic paths (`states.a.on.GO[1]`) are tracked as a borrowed linked list and only
//! rendered when a warning or error is produced, so the happy path allocates nothing for them.

use std::fmt;

use serde_json::{Map, Value};

use super::{
    ActionConfig, ActionErrorPolicy, ConfigWarning, GuardConfig, GuardErrorPolicy, InvokeConfig,
    MachineConfig, Policies, StateConfig, StateKind, TransitionConfig, UnhandledPolicy,
};
use crate::error::{Error, Result};

/// Parser options.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// Override the document's `strictConfig` (unknown keys become errors).
    pub strict_config: Option<bool>,
}

const STATE_KEYS: &[&str] = &[
    "id",
    "initial",
    "states",
    "type",
    "output",
    "on",
    "entry",
    "exit",
    "after",
    "always",
    "invoke",
    "onDone",
    "history",
    "target",
    "meta",
    "description",
    "tags",
];
const ROOT_ONLY_KEYS: &[&str] = &[
    "context",
    "version",
    "actionErrorPolicy",
    "guardErrorPolicy",
    "onUnhandled",
    "maxIterations",
    "spawnBlockingTimeout",
    "strict",
    "strictTargets",
    "strictConfig",
    // Emitted by the Stately editor / TypeScript tooling; carry no runtime meaning here.
    "types",
    "schemas",
    "$schema",
    "predictableActionArguments",
    "preserveActionOrder",
];
const TRANSITION_KEYS: &[&str] =
    &["target", "actions", "guard", "cond", "internal", "reenter", "meta", "description", "tags"];
const INVOKE_KEYS: &[&str] =
    &["id", "src", "input", "systemId", "onDone", "onError", "meta", "description"];

/// A location inside the configuration document, rendered lazily.
#[derive(Clone, Copy)]
pub(crate) struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    seg: Seg<'a>,
}

#[derive(Clone, Copy)]
enum Seg<'a> {
    Root,
    Key(&'a str),
    Index(usize),
}

impl<'a> Path<'a> {
    const ROOT: Path<'static> = Path { parent: None, seg: Seg::Root };

    #[inline]
    fn key(&'a self, key: &'a str) -> Path<'a> {
        Path { parent: Some(self), seg: Seg::Key(key) }
    }

    #[inline]
    fn index(&'a self, index: usize) -> Path<'a> {
        Path { parent: Some(self), seg: Seg::Index(index) }
    }

    fn render(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(parent) = self.parent {
            parent.fmt(f)?;
        }
        match self.seg {
            Seg::Root => Ok(()),
            Seg::Key(k) => {
                if self.parent.is_some_and(|p| !matches!(p.seg, Seg::Root)) {
                    f.write_str(".")?;
                }
                f.write_str(k)
            }
            Seg::Index(i) => write!(f, "[{i}]"),
        }
    }
}

fn expect_object<'v>(
    value: &'v Value,
    path: &Path<'_>,
    what: &str,
) -> Result<&'v Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Error::config(path.render(), format!("{what} must be an object")))
}

struct Parser {
    strict_config: bool,
    warnings: Vec<ConfigWarning>,
}

pub(super) fn parse_machine(value: &Value, options: &ParseOptions) -> Result<MachineConfig> {
    let root_path = Path::ROOT;
    let obj = expect_object(value, &root_path, "machine config")?;
    let strict_config = options
        .strict_config
        .or_else(|| obj.get("strictConfig").and_then(Value::as_bool))
        .unwrap_or(false);
    let mut parser = Parser { strict_config, warnings: Vec::new() };

    let id = match obj.get("id") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return Err(Error::config("id", "machine `id` must be a non-empty string")),
        None => return Err(Error::config("id", "machine config requires an `id`")),
    };
    if !obj.contains_key("states") {
        return Err(Error::config("states", "machine config requires `states`"));
    }

    parser.check_keys(obj, &root_path, STATE_KEYS, ROOT_ONLY_KEYS)?;
    let root = parser.parse_state(obj, id, &root_path)?;
    let context = obj.get("context").cloned().unwrap_or(Value::Null);
    if context.is_string() {
        parser.warn(
            &root_path.key("context"),
            "string `context` (editor placeholder?) treated as `{}`",
        );
    }
    let context = if context.is_string() { Value::Object(Map::new()) } else { context };
    let version = match obj.get("version") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return Err(Error::config("version", "`version` must be a string or number")),
    };
    let policies = parser.parse_policies(obj)?;
    Ok(MachineConfig { root, context, version, policies, warnings: parser.warnings })
}

impl Parser {
    fn warn(&mut self, path: &Path<'_>, message: impl Into<String>) {
        self.warnings.push(ConfigWarning { path: path.render(), message: message.into() });
    }

    fn check_keys(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
        allowed: &[&str],
        extra: &[&str],
    ) -> Result<()> {
        for key in obj.keys() {
            if allowed.contains(&key.as_str())
                || extra.contains(&key.as_str())
                || key.starts_with("x-")
            {
                continue;
            }
            let hint = closest(key, allowed.iter().chain(extra).copied());
            let message = match hint {
                Some(h) => format!("unknown key `{key}` (did you mean `{h}`?)"),
                None => format!("unknown key `{key}`"),
            };
            if self.strict_config {
                return Err(Error::config(path.key(key).render(), message));
            }
            self.warn(&path.key(key), message);
        }
        Ok(())
    }

    fn parse_policies(&mut self, obj: &Map<String, Value>) -> Result<Policies> {
        let mut p = Policies { strict_config: self.strict_config, ..Policies::default() };
        if let Some(v) = obj.get("strict") {
            p.strict =
                v.as_bool().ok_or_else(|| Error::config("strict", "`strict` must be a boolean"))?;
        }
        if let Some(v) = obj.get("strictTargets") {
            p.strict_targets = v.as_bool().ok_or_else(|| {
                Error::config("strictTargets", "`strictTargets` must be a boolean")
            })?;
        }
        if let Some(v) = obj.get("maxIterations") {
            let n =
                v.as_u64().filter(|n| *n >= 1 && u32::try_from(*n).is_ok()).ok_or_else(|| {
                    Error::config("maxIterations", "`maxIterations` must be a positive integer")
                })?;
            #[allow(clippy::cast_possible_truncation)]
            {
                p.max_iterations = n as u32;
            }
        }
        if let Some(v) = obj.get("actionErrorPolicy") {
            p.action_error_policy = match v.as_str() {
                Some("continue") => ActionErrorPolicy::Continue,
                Some("rollback") => ActionErrorPolicy::Rollback,
                Some("fail") => ActionErrorPolicy::Fail,
                _ => {
                    return Err(Error::config(
                        "actionErrorPolicy",
                        "`actionErrorPolicy` must be \"continue\", \"rollback\" or \"fail\"",
                    ));
                }
            };
        }
        if let Some(v) = obj.get("guardErrorPolicy") {
            p.guard_error_policy = match v {
                Value::Bool(false) => GuardErrorPolicy::False,
                Value::Bool(true) => GuardErrorPolicy::True,
                Value::String(s) if s == "raise" => GuardErrorPolicy::Raise,
                _ => {
                    return Err(Error::config(
                        "guardErrorPolicy",
                        "`guardErrorPolicy` must be false, true or \"raise\"",
                    ));
                }
            };
        }
        if let Some(v) = obj.get("onUnhandled") {
            p.on_unhandled = match v.as_str() {
                Some("ignore") => UnhandledPolicy::Ignore,
                Some("defer") => UnhandledPolicy::Defer,
                Some("error") => UnhandledPolicy::Error,
                _ => {
                    return Err(Error::config(
                        "onUnhandled",
                        "`onUnhandled` must be \"ignore\", \"defer\" or \"error\"",
                    ));
                }
            };
        }
        Ok(p)
    }

    fn parse_state(
        &mut self,
        obj: &Map<String, Value>,
        key: String,
        path: &Path<'_>,
    ) -> Result<StateConfig> {
        let mut state = StateConfig::new(key);
        let is_root = matches!(path.seg, Seg::Root);

        if let (Some(v), false) = (obj.get("id"), is_root) {
            let id = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                Error::config(path.key("id").render(), "state `id` must be a non-empty string")
            })?;
            state.custom_id = Some(id.to_owned());
        }

        // Children first: the kind depends on whether there are any.
        if let Some(children) = obj.get("states") {
            let states_path = path.key("states");
            let map = expect_object(children, &states_path, "`states`")?;
            for (child_key, child_val) in map {
                let child_path = states_path.key(child_key);
                if child_key.is_empty() {
                    return Err(Error::config(child_path.render(), "state keys must be non-empty"));
                }
                let child_obj = expect_object(child_val, &child_path, "state")?;
                self.check_keys(child_obj, &child_path, STATE_KEYS, &[])?;
                state.states.push(self.parse_state(child_obj, child_key.clone(), &child_path)?);
            }
        }

        state.kind = self.parse_kind(obj, path, state.states.is_empty())?;
        state.initial = Self::parse_initial(obj, path, &state)?;
        self.parse_handlers(obj, path, &mut state)?;
        Self::parse_metadata(obj, path, &mut state)?;
        Ok(state)
    }

    fn parse_kind(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
        leaf: bool,
    ) -> Result<StateKind> {
        let declared_type = match obj.get("type") {
            None => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => {
                return Err(Error::config(path.key("type").render(), "`type` must be a string"));
            }
        };
        Ok(match (declared_type, leaf) {
            (Some("parallel"), _) => StateKind::Parallel,
            (Some("final"), _) => StateKind::Final,
            (Some("history"), _) => {
                let deep = match obj.get("history") {
                    None => false,
                    Some(Value::String(s)) if s == "deep" => true,
                    Some(Value::String(s)) if s == "shallow" => false,
                    Some(_) => {
                        return Err(Error::config(
                            path.key("history").render(),
                            "`history` must be \"shallow\" or \"deep\"",
                        ));
                    }
                };
                let target = match obj.get("target") {
                    None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => {
                        return Err(Error::config(
                            path.key("target").render(),
                            "history `target` must be a string",
                        ));
                    }
                };
                StateKind::History { deep, target }
            }
            (Some("compound") | None, false) => StateKind::Compound,
            (Some("atomic") | None, true) => StateKind::Atomic,
            (Some("compound"), true) => {
                self.warn(&path.key("type"), "`compound` state without children treated as atomic");
                StateKind::Atomic
            }
            (Some("atomic"), false) => {
                self.warn(&path.key("type"), "`atomic` state with children treated as compound");
                StateKind::Compound
            }
            (Some(other), _) => {
                return Err(Error::config(
                    path.key("type").render(),
                    format!("unknown state type `{other}`"),
                ));
            }
        })
    }

    fn parse_initial(
        obj: &Map<String, Value>,
        path: &Path<'_>,
        state: &StateConfig,
    ) -> Result<Option<String>> {
        let initial = match obj.get("initial") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(
                    path.key("initial").render(),
                    "`initial` must be a string",
                ));
            }
        };
        if state.kind != StateKind::Compound || initial.is_some() {
            return Ok(initial);
        }
        let mut non_history =
            state.states.iter().filter(|s| !matches!(s.kind, StateKind::History { .. }));
        match (non_history.next(), non_history.next()) {
            (Some(only), None) => Ok(Some(only.key.clone())),
            _ => Err(Error::config(
                path.key("initial").render(),
                "compound state needs an `initial` child",
            )),
        }
    }

    fn parse_handlers(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
        state: &mut StateConfig,
    ) -> Result<()> {
        if let Some(on) = obj.get("on") {
            let on_path = path.key("on");
            let map = expect_object(on, &on_path, "`on`")?;
            for (descriptor, v) in map {
                let transitions = self.parse_transitions(v, &on_path.key(descriptor))?;
                if descriptor.is_empty() {
                    state.always.extend(transitions);
                } else {
                    state.on.push((descriptor.clone(), transitions));
                }
            }
        }
        if let Some(always) = obj.get("always") {
            state.always.extend(self.parse_transitions(always, &path.key("always"))?);
        }
        if let Some(after) = obj.get("after") {
            let after_path = path.key("after");
            let map = expect_object(after, &after_path, "`after`")?;
            for (delay_key, v) in map {
                let transitions = self.parse_transitions(v, &after_path.key(delay_key))?;
                state.after.push((delay_key.clone(), transitions));
            }
        }
        if let Some(v) = obj.get("onDone") {
            state.on_done = self.parse_transitions(v, &path.key("onDone"))?;
        }
        state.entry = parse_actions(obj.get("entry"), &path.key("entry"))?;
        state.exit = parse_actions(obj.get("exit"), &path.key("exit"))?;

        if let Some(inv) = obj.get("invoke") {
            let invoke_path = path.key("invoke");
            match inv {
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        state.invoke.push(self.parse_invoke(item, &invoke_path.index(i))?);
                    }
                }
                single => state.invoke.push(self.parse_invoke(single, &invoke_path)?),
            }
        }
        Ok(())
    }

    fn parse_metadata(
        obj: &Map<String, Value>,
        path: &Path<'_>,
        state: &mut StateConfig,
    ) -> Result<()> {
        state.output = obj.get("output").cloned();
        state.meta = match obj.get("meta") {
            None => None,
            Some(v @ Value::Object(_)) => Some(v.clone()),
            Some(_) => {
                return Err(Error::config(path.key("meta").render(), "`meta` must be an object"));
            }
        };
        state.description = match obj.get("description") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(
                    path.key("description").render(),
                    "`description` must be a string",
                ));
            }
        };
        state.tags = parse_tags(obj.get("tags"), &path.key("tags"))?;
        Ok(())
    }

    fn parse_transitions(
        &mut self,
        value: &Value,
        path: &Path<'_>,
    ) -> Result<Vec<TransitionConfig>> {
        match value {
            Value::Null => {
                Ok(vec![TransitionConfig { forbidden: true, ..TransitionConfig::default() }])
            }
            Value::String(target) => {
                Ok(vec![TransitionConfig { targets: vec![target.clone()], ..Default::default() }])
            }
            Value::Object(obj) => Ok(vec![self.parse_transition(obj, path)?]),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let p = path.index(i);
                    match item {
                        Value::String(target) => {
                            out.push(TransitionConfig {
                                targets: vec![target.clone()],
                                ..Default::default()
                            });
                        }
                        Value::Object(obj) => out.push(self.parse_transition(obj, &p)?),
                        _ => {
                            return Err(Error::config(
                                p.render(),
                                "transition must be a string or an object",
                            ));
                        }
                    }
                }
                Ok(out)
            }
            _ => Err(Error::config(
                path.render(),
                "transition must be a string, object, array or null",
            )),
        }
    }

    fn parse_transition(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
    ) -> Result<TransitionConfig> {
        self.check_keys(obj, path, TRANSITION_KEYS, &[])?;
        // An empty object is XState's own spelling of a forbidden transition.
        if obj.is_empty() {
            return Ok(TransitionConfig { forbidden: true, ..TransitionConfig::default() });
        }
        let targets = match obj.get("target") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .map(|t| {
                    t.as_str().map(str::to_owned).ok_or_else(|| {
                        Error::config(path.key("target").render(), "targets must be strings")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => {
                return Err(Error::config(
                    path.key("target").render(),
                    "`target` must be a string or array of strings",
                ));
            }
        };
        let actions = parse_actions(obj.get("actions"), &path.key("actions"))?;
        let guard = match (obj.get("guard"), obj.get("cond")) {
            (Some(g), _) | (None, Some(g)) => Some(parse_guard(g, &path.key("guard"))?),
            (None, None) => None,
        };
        let reenter = match (obj.get("reenter"), obj.get("internal")) {
            (Some(v), _) => v.as_bool().ok_or_else(|| {
                Error::config(path.key("reenter").render(), "`reenter` must be a boolean")
            })?,
            (None, Some(v)) => !v.as_bool().ok_or_else(|| {
                Error::config(path.key("internal").render(), "`internal` must be a boolean")
            })?,
            (None, None) => false,
        };
        let meta = obj.get("meta").cloned();
        let description = obj.get("description").and_then(Value::as_str).map(str::to_owned);
        Ok(TransitionConfig {
            targets,
            actions,
            guard,
            reenter,
            forbidden: false,
            meta,
            description,
        })
    }

    fn parse_invoke(&mut self, value: &Value, path: &Path<'_>) -> Result<InvokeConfig> {
        let obj = expect_object(value, path, "`invoke`")?;
        self.check_keys(obj, path, INVOKE_KEYS, &[])?;
        let src = match obj.get("src") {
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
        let id = match obj.get("id") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(path.key("id").render(), "invoke `id` must be a string"));
            }
        };
        let system_id = match obj.get("systemId") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(
                    path.key("systemId").render(),
                    "`systemId` must be a string",
                ));
            }
        };
        let on_done = match obj.get("onDone") {
            Some(v) => self.parse_transitions(v, &path.key("onDone"))?,
            None => Vec::new(),
        };
        let on_error = match obj.get("onError") {
            Some(v) => self.parse_transitions(v, &path.key("onError"))?,
            None => Vec::new(),
        };
        Ok(InvokeConfig {
            id,
            src,
            input: obj.get("input").cloned().unwrap_or(Value::Null),
            system_id,
            on_done,
            on_error,
        })
    }
}

pub(super) fn parse_actions(value: Option<&Value>, path: &Path<'_>) -> Result<Vec<ActionConfig>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => {
            items.iter().enumerate().map(|(i, item)| parse_action(item, &path.index(i))).collect()
        }
        single => Ok(vec![parse_action(single, path)?]),
    }
}

fn parse_action(item: &Value, path: &Path<'_>) -> Result<ActionConfig> {
    match item {
        Value::String(name) if !name.is_empty() => {
            Ok(ActionConfig { name: name.clone(), params: Value::Null })
        }
        Value::Object(obj) => {
            let name =
                obj.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(
                    || Error::config(path.render(), "action object needs a string `type`"),
                )?;
            for key in obj.keys() {
                if key != "type" && key != "params" {
                    return Err(Error::config(
                        path.render(),
                        format!(
                            "unexpected key `{key}` on action `{name}`; built-in options go under `params`"
                        ),
                    ));
                }
            }
            Ok(ActionConfig {
                name: name.to_owned(),
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            })
        }
        _ => Err(Error::config(
            path.render(),
            "action must be a non-empty string or `{type, params}`",
        )),
    }
}

pub(super) fn parse_guard(value: &Value, path: &Path<'_>) -> Result<GuardConfig> {
    match value {
        Value::String(s) => {
            if let Some(inner) = s.strip_prefix('!') {
                if inner.is_empty() {
                    return Err(Error::config(
                        path.render(),
                        "`!` must be followed by a guard name",
                    ));
                }
                return Ok(GuardConfig::Not(Box::new(GuardConfig::Named {
                    name: inner.to_owned(),
                    params: Value::Null,
                })));
            }
            if s.is_empty() {
                return Err(Error::config(path.render(), "guard name must be non-empty"));
            }
            Ok(GuardConfig::Named { name: s.clone(), params: Value::Null })
        }
        Value::Object(obj) => {
            let ty = obj.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(
                || Error::config(path.render(), "guard object needs a string `type`"),
            )?;
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            let operands = || -> Result<Vec<GuardConfig>> {
                let list = obj
                    .get("children")
                    .or_else(|| params.get("guards"))
                    .or_else(|| params.get("children"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        Error::config(
                            path.render(),
                            format!(
                                "`{ty}` guard needs operands under `children` or `params.guards`"
                            ),
                        )
                    })?;
                list.iter().enumerate().map(|(i, g)| parse_guard(g, &path.index(i))).collect()
            };
            match ty {
                "and" => Ok(GuardConfig::And(operands()?)),
                "or" => Ok(GuardConfig::Or(operands()?)),
                "not" => {
                    let inner = params
                        .get("guard")
                        .or_else(|| {
                            obj.get("children").and_then(|c| c.as_array()).and_then(|a| a.first())
                        })
                        .ok_or_else(|| {
                            Error::config(
                                path.render(),
                                "`not` guard needs `params.guard` or one child",
                            )
                        })?;
                    Ok(GuardConfig::Not(Box::new(parse_guard(inner, &path.key("guard"))?)))
                }
                _ => Ok(GuardConfig::Named { name: ty.to_owned(), params }),
            }
        }
        _ => Err(Error::config(path.render(), "guard must be a string or an object")),
    }
}

/// Parse a guard written as a detached JSON value (used by the `choose` compiler).
pub(crate) fn parse_guard_at(value: &Value, path: &str) -> Result<GuardConfig> {
    let root = Path::ROOT;
    parse_guard(value, &root.key(path))
}

/// Parse an action list written as a detached JSON value (used by the `choose` compiler).
pub(crate) fn parse_actions_at(value: Option<&Value>, path: &str) -> Result<Vec<ActionConfig>> {
    let root = Path::ROOT;
    parse_actions(value, &root.key(path))
}

fn parse_tags(value: Option<&Value>, path: &Path<'_>) -> Result<Vec<String>> {
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
fn closest<'a>(key: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> MachineConfig {
        MachineConfig::from_json(text).expect("parses")
    }

    #[test]
    fn shorthands_expand() {
        let cfg = parse(
            r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b","STAY":null,"X":[{"target":"b","cond":"g"},"a"]}},"b":{}}}"#,
        );
        let a = &cfg.root.states[0];
        assert_eq!(a.on.len(), 3);
        assert_eq!(a.on[0].1[0].targets, vec!["b"]);
        assert!(a.on[1].1[0].forbidden);
        assert_eq!(a.on[2].1.len(), 2);
        assert_eq!(
            a.on[2].1[0].guard,
            Some(GuardConfig::Named { name: "g".into(), params: Value::Null })
        );
    }

    #[test]
    fn inferred_initial_and_kinds() {
        let cfg = parse(
            r#"{"id":"m","initial":"p","states":{"p":{"type":"parallel","states":{"r":{"states":{"only":{}}}}},"h":{"type":"history","history":"deep"},"f":{"type":"final"}}}"#,
        );
        assert_eq!(cfg.root.states[0].kind, StateKind::Parallel);
        assert_eq!(cfg.root.states[0].states[0].initial.as_deref(), Some("only"));
        assert_eq!(cfg.root.states[1].kind, StateKind::History { deep: true, target: None });
        assert_eq!(cfg.root.states[2].kind, StateKind::Final);
    }

    #[test]
    fn unknown_keys_warn_or_fail_with_paths() {
        let cfg = parse(r#"{"id":"m","initial":"a","states":{"a":{"entri":"x"}}}"#);
        assert_eq!(cfg.warnings.len(), 1);
        assert_eq!(cfg.warnings[0].path, "states.a.entri");
        assert!(cfg.warnings[0].message.contains("entry"));
        let err = MachineConfig::from_json(
            r#"{"id":"m","strictConfig":true,"initial":"a","states":{"a":{"on":{"GO":[{"target":"b","gard":"x"}]}},"b":{}}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("states.a.on.GO[0].gard"), "{err}");
    }

    #[test]
    fn built_in_param_shape_is_enforced() {
        let err = MachineConfig::from_json(
            r#"{"id":"m","initial":"a","states":{"a":{"entry":{"type":"raise","event":"X"}}}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("params"));
    }
}
