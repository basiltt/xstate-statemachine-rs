//! Lenient shape parser for XState v5 JSON.
//!
//! Diagnostic paths (`states.a.on.GO[1]`) are tracked as a borrowed linked list and only
//! rendered when a warning or error is produced, so the happy path allocates nothing for them.
//! Error messages also name the state (`State 'm.a' entry: ...`), as the Python library does.

use std::collections::HashSet;
use std::fmt;

use serde_json::{Map, Value};

use super::{
    ActionErrorPolicy, ConfigWarning, GuardErrorPolicy, MachineConfig, Policies, StateConfig,
    StateKind, UnhandledPolicy,
};
use crate::error::{Error, Result};

mod items;

pub(crate) use items::{parse_actions_at, parse_guard_at};

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
const STATE_TYPES: &[&str] = &["atomic", "compound", "parallel", "final", "history"];

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

/// `obj[key]`. Configuration objects hold a handful of keys, so a scan comparing lengths
/// first beats hashing the key (`serde_json` maps hash with `SipHash`); large maps still use
/// the index.
#[inline]
pub(crate) fn field<'v>(obj: &'v Map<String, Value>, key: &str) -> Option<&'v Value> {
    const SCAN_LIMIT: usize = 12;
    if obj.len() > SCAN_LIMIT {
        return obj.get(key);
    }
    obj.iter().find_map(|(k, v)| (k == key).then_some(v))
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

/// Prefix the message of a configuration error with its owner (`State 'm.a' entry`), the way
/// the Python library names where a malformed action, guard or transition was found.
fn within(err: Error, owner: impl FnOnce() -> String) -> Error {
    match err {
        Error::InvalidConfig { path, message } => {
            Error::InvalidConfig { path, message: format!("{}: {message}", owner()) }
        }
        other => other,
    }
}

pub(super) struct Parser<'o> {
    strict_config: bool,
    warnings: Vec<ConfigWarning>,
    /// Action names the machine's logic implements: such a name is the user's action even
    /// when it is spelled like a built-in, so the built-in's `params` keys are not checked.
    user_actions: Option<&'o HashSet<String>>,
}

pub(super) fn parse_machine(
    value: &Value,
    options: &ParseOptions,
    user_actions: Option<&HashSet<String>>,
) -> Result<MachineConfig> {
    parse_machine_with(value, options, user_actions, true)
}

/// [`parse_machine`]; `keep_source: false` leaves [`MachineConfig::source`] `null`.
pub(super) fn parse_machine_with(
    value: &Value,
    options: &ParseOptions,
    user_actions: Option<&HashSet<String>>,
    keep_source: bool,
) -> Result<MachineConfig> {
    let root_path = Path::ROOT;
    let obj = expect_object(value, &root_path, "machine config")?;
    let mut id = match field(obj, "id") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return Err(Error::config("id", "machine `id` must be a non-empty string")),
        None => return Err(Error::config("id", "machine config requires an `id`")),
    };
    let declared_strict = flag(obj, "strictConfig", &id)?;
    let strict_config = options.strict_config.or(declared_strict).unwrap_or(false);
    let mut parser = Parser { strict_config, warnings: Vec::new(), user_actions };
    if field(obj, "states").is_none() {
        return Err(Error::config("states", "machine config requires `states`"));
    }

    parser.check_keys(obj, &root_path, STATE_KEYS, ROOT_ONLY_KEYS)?;
    let root = parser.parse_state(obj, id.clone(), &mut id, &root_path)?;
    let context = field(obj, "context").cloned().unwrap_or(Value::Null);
    if context.is_string() {
        parser.warn(
            &root_path.key("context"),
            "string `context` (editor placeholder?) treated as `{}`",
        );
    }
    let context = if context.is_string() { Value::Object(Map::new()) } else { context };
    let version = match field(obj, "version") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return Err(Error::config("version", "`version` must be a string or number")),
    };
    let policies = parser.parse_policies(obj, &id)?;
    Ok(MachineConfig {
        root,
        context,
        version,
        policies,
        warnings: parser.warnings,
        source: if keep_source { value.clone() } else { Value::Null },
    })
}

/// A root boolean flag: JSON `true` / `false`, or `null` / absent for "not set". A string such
/// as `"false"` is refused rather than read as truthy.
fn flag(obj: &Map<String, Value>, key: &str, machine: &str) -> Result<Option<bool>> {
    match field(obj, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(Error::config(
            key,
            format!("Machine '{machine}': `{key}` must be true or false, got {other}"),
        )),
    }
}

/// A whole number written as an integer or an integral float (`5`, `5.0`).
fn whole_number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let f = value.as_f64()?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f < 1.8e19).then_some(f as u64)
    })
}

impl Parser<'_> {
    fn warn(&mut self, path: &Path<'_>, message: impl Into<String>) {
        self.warnings.push(ConfigWarning { path: path.render(), message: message.into() });
    }

    /// Report an unknown key: an error under `strictConfig`, a warning otherwise.
    fn report(&mut self, path: &Path<'_>, message: String) -> Result<()> {
        if self.strict_config {
            return Err(Error::config(path.render(), message));
        }
        self.warn(path, message);
        Ok(())
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
            let hint = items::closest(key, allowed.iter().chain(extra).copied());
            let message = match hint {
                Some(h) => format!("unknown key `{key}` (did you mean `{h}`?)"),
                None => format!("unknown key `{key}`"),
            };
            self.report(&path.key(key), message)?;
        }
        Ok(())
    }

    fn parse_policies(&mut self, obj: &Map<String, Value>, id: &str) -> Result<Policies> {
        let mut p = Policies { strict_config: self.strict_config, ..Policies::default() };
        if let Some(v) = flag(obj, "strict", id)? {
            p.strict = v;
        }
        if let Some(v) = flag(obj, "strictTargets", id)? {
            p.strict_targets = v;
        }
        if let Some(v) = field(obj, "maxIterations") {
            let n = whole_number(v).filter(|n| *n >= 1).and_then(|n| u32::try_from(n).ok());
            p.max_iterations = n.ok_or_else(|| {
                Error::config(
                    "maxIterations",
                    format!("Machine '{id}': `maxIterations` must be a whole number >= 1, got {v}"),
                )
            })?;
        }
        match field(obj, "spawnBlockingTimeout") {
            None | Some(Value::Null) => {}
            Some(v) if v.as_f64().is_some_and(|ms| ms.is_finite() && ms >= 0.0) => {}
            Some(v) => {
                return Err(Error::config(
                    "spawnBlockingTimeout",
                    format!(
                        "Machine '{id}': `spawnBlockingTimeout` must be a finite number >= 0, got {v}"
                    ),
                ));
            }
        }
        Self::parse_policy_names(obj, &mut p)?;
        Ok(p)
    }

    /// The string-valued policies: `actionErrorPolicy`, `guardErrorPolicy`, `onUnhandled`.
    fn parse_policy_names(obj: &Map<String, Value>, p: &mut Policies) -> Result<()> {
        if let Some(v) = field(obj, "actionErrorPolicy") {
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
        if let Some(v) = field(obj, "guardErrorPolicy") {
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
        if let Some(v) = field(obj, "onUnhandled") {
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
        Ok(())
    }

    /// Parse one state node. `id` holds the node's full id; it is extended in place for the
    /// children and restored before they return.
    fn parse_state(
        &mut self,
        obj: &Map<String, Value>,
        key: String,
        id: &mut String,
        path: &Path<'_>,
    ) -> Result<StateConfig> {
        let mut state = StateConfig::new(key);
        let is_root = matches!(path.seg, Seg::Root);

        if let (Some(v), false) = (field(obj, "id"), is_root) {
            let custom = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                Error::config(path.key("id").render(), "state `id` must be a non-empty string")
            })?;
            state.custom_id = Some(custom.to_owned());
        }

        // Children first: the kind depends on whether there are any.
        if let Some(children) = field(obj, "states") {
            let states_path = path.key("states");
            let map = expect_object(children, &states_path, "`states`")?;
            for (child_key, child_val) in map {
                let child_path = states_path.key(child_key);
                if child_key.is_empty() {
                    return Err(Error::config(
                        child_path.render(),
                        format!(
                            "State '{id}' has an empty child key; state keys must be non-empty"
                        ),
                    ));
                }
                let child_obj = expect_object(child_val, &child_path, "state")?;
                self.check_keys(child_obj, &child_path, STATE_KEYS, &[])?;
                let len = id.len();
                id.push('.');
                id.push_str(child_key);
                let child = self.parse_state(child_obj, child_key.clone(), id, &child_path)?;
                id.truncate(len);
                state.states.push(child);
            }
        }

        let id = id.as_str();
        state.kind = self.parse_kind(obj, path, id, !state.states.is_empty())?;
        state.initial = Self::parse_initial(obj, path, id, &state)?;
        self.parse_handlers(obj, path, id, &mut state)?;
        Self::parse_metadata(obj, path, &mut state)?;
        Ok(state)
    }

    /// The declared `type`, validated against the node's shape. An unknown type (`"Final"`,
    /// `"paralel"`) and a `compound` / `parallel` state without children are errors; an
    /// `atomic` / `final` / `history` state with children is built as compound, with a warning.
    fn parse_kind(
        &mut self,
        obj: &Map<String, Value>,
        path: &Path<'_>,
        id: &str,
        has_children: bool,
    ) -> Result<StateKind> {
        let ty = match field(obj, "type") {
            None | Some(Value::Null) => {
                return Ok(if has_children { StateKind::Compound } else { StateKind::Atomic });
            }
            Some(Value::String(s)) if STATE_TYPES.contains(&s.as_str()) => s.as_str(),
            Some(other) => {
                return Err(Error::config(
                    path.key("type").render(),
                    format!(
                        "State '{id}' has an invalid 'type' {other}; expected one of {STATE_TYPES:?}"
                    ),
                ));
            }
        };
        let container = matches!(ty, "compound" | "parallel");
        if has_children && !container {
            self.warn(
                &path.key("type"),
                format!(
                    "State '{id}' is declared 'type': \"{ty}\" but has child 'states'; only \
                     'compound' and 'parallel' states may have children. Building it as \
                     'compound' and ignoring the declared type."
                ),
            );
            return Ok(StateKind::Compound);
        }
        if !has_children && container {
            return Err(Error::config(
                path.key("type").render(),
                format!("State '{id}' is declared 'type': \"{ty}\" but has no child 'states'"),
            ));
        }
        Ok(match ty {
            "atomic" => StateKind::Atomic,
            "compound" => StateKind::Compound,
            "parallel" => StateKind::Parallel,
            "final" => StateKind::Final,
            _ => Self::parse_history(obj, path, id)?,
        })
    }

    fn parse_history(obj: &Map<String, Value>, path: &Path<'_>, id: &str) -> Result<StateKind> {
        let deep = match field(obj, "history") {
            None | Some(Value::Null) => false,
            Some(Value::String(s)) if s == "deep" => true,
            Some(Value::String(s)) if s == "shallow" => false,
            Some(other) => {
                return Err(Error::config(
                    path.key("history").render(),
                    format!("State '{id}': 'history' must be \"shallow\" or \"deep\", got {other}"),
                ));
            }
        };
        let target = match field(obj, "target") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(other) => {
                return Err(Error::config(
                    path.key("target").render(),
                    format!(
                        "State '{id}' has an invalid 'target' {other}; a history state's default \
                         'target' must be a non-empty state name"
                    ),
                ));
            }
        };
        Ok(StateKind::History { deep, target })
    }

    fn parse_initial(
        obj: &Map<String, Value>,
        path: &Path<'_>,
        id: &str,
        state: &StateConfig,
    ) -> Result<Option<String>> {
        let initial = match field(obj, "initial") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(
                    path.key("initial").render(),
                    "`initial` must be a string",
                ));
            }
        };
        if state.kind != StateKind::Compound {
            return Ok(initial);
        }
        if let Some(key) = &initial {
            if state.states.iter().any(|c| c.key == *key) {
                return Ok(initial);
            }
            let hint = items::closest(key, state.states.iter().map(|c| c.key.as_str()))
                .map_or_else(String::new, |h| format!(" Did you mean '{h}'?"));
            let mut children: Vec<&str> = state.states.iter().map(|c| c.key.as_str()).collect();
            children.sort_unstable();
            return Err(Error::config(
                path.key("initial").render(),
                format!(
                    "State '{id}': 'initial' '{key}' is not one of its child states {children:?}.{hint}"
                ),
            ));
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
        id: &str,
        state: &mut StateConfig,
    ) -> Result<()> {
        if let Some(on) = field(obj, "on") {
            let on_path = path.key("on");
            let map = expect_object(on, &on_path, "`on`")?;
            for (descriptor, v) in map {
                let owner = items::Owner { state: id, label: items::Label::On(descriptor) };
                let transitions = self.parse_transitions(v, &on_path.key(descriptor), &owner)?;
                if descriptor.is_empty() {
                    state.always.extend(transitions);
                } else {
                    state.on.push((descriptor.clone(), transitions));
                }
            }
        }
        if let Some(always) = field(obj, "always") {
            let owner = items::Owner { state: id, label: items::Label::Key("always") };
            state.always.extend(self.parse_transitions(always, &path.key("always"), &owner)?);
        }
        if let Some(after) = field(obj, "after") {
            let after_path = path.key("after");
            let map = expect_object(after, &after_path, "`after`")?;
            for (delay_key, v) in map {
                let owner = items::Owner { state: id, label: items::Label::After(delay_key) };
                let transitions = self.parse_transitions(v, &after_path.key(delay_key), &owner)?;
                state.after.push((delay_key.clone(), transitions));
            }
        }
        if let Some(v) = field(obj, "onDone") {
            let owner = items::Owner { state: id, label: items::Label::Key("onDone") };
            state.on_done = self.parse_transitions(v, &path.key("onDone"), &owner)?;
        }
        state.entry = self
            .parse_actions(field(obj, "entry"), &path.key("entry"))
            .map_err(|e| within(e, || format!("State '{id}' entry")))?;
        state.exit = self
            .parse_actions(field(obj, "exit"), &path.key("exit"))
            .map_err(|e| within(e, || format!("State '{id}' exit")))?;

        if let Some(inv) = field(obj, "invoke") {
            state.invoke = self.parse_invokes(inv, &path.key("invoke"), id)?;
        }
        Ok(())
    }

    fn parse_invokes(
        &mut self,
        value: &Value,
        path: &Path<'_>,
        id: &str,
    ) -> Result<Vec<super::InvokeConfig>> {
        match value {
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| self.parse_invoke(item, &path.index(i), id))
                .collect(),
            single => Ok(vec![self.parse_invoke(single, path, id)?]),
        }
    }

    fn parse_metadata(
        obj: &Map<String, Value>,
        path: &Path<'_>,
        state: &mut StateConfig,
    ) -> Result<()> {
        state.output = field(obj, "output").cloned();
        state.meta = match field(obj, "meta") {
            None => None,
            Some(v @ Value::Object(_)) => Some(v.clone()),
            Some(_) => {
                return Err(Error::config(path.key("meta").render(), "`meta` must be an object"));
            }
        };
        state.description = match field(obj, "description") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(Error::config(
                    path.key("description").render(),
                    "`description` must be a string",
                ));
            }
        };
        state.tags = items::parse_tags(field(obj, "tags"), &path.key("tags"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
