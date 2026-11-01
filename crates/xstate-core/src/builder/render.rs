//! Renders a finished builder configuration back into the JSON document the parser would
//! accept for it. That document is stored as [`MachineConfig::source`] so that
//! [`Machine::to_json`](crate::Machine::to_json) works for builder-made machines too, and
//! feeding it back to the parser yields the same configuration tree.

use serde_json::{Map, Value, json};

use crate::config::{
    ActionConfig, ActionErrorPolicy, GuardConfig, GuardErrorPolicy, InvokeConfig, Policies,
    StateConfig, StateKind, TransitionConfig, UnhandledPolicy,
};

/// The full machine document: root state keys, context, version and non-default policies.
pub(super) fn machine_document(
    root: &StateConfig,
    context: &Value,
    version: Option<&str>,
    policies: &Policies,
) -> Value {
    let mut doc = state_object(root);
    // The root's own key is the machine id.
    let mut out = Map::new();
    out.insert("id".to_owned(), Value::String(root.key.clone()));
    if !context.is_null() {
        out.insert("context".to_owned(), context.clone());
    }
    if let Some(version) = version {
        out.insert("version".to_owned(), Value::String(version.to_owned()));
    }
    out.extend(policy_entries(policies));
    out.append(&mut doc);
    Value::Object(out)
}

fn policy_entries(policies: &Policies) -> Map<String, Value> {
    let default = Policies::default();
    let mut out = Map::new();
    if policies.strict != default.strict {
        out.insert("strict".to_owned(), json!(policies.strict));
    }
    if policies.strict_targets != default.strict_targets {
        out.insert("strictTargets".to_owned(), json!(policies.strict_targets));
    }
    if policies.strict_config != default.strict_config {
        out.insert("strictConfig".to_owned(), json!(policies.strict_config));
    }
    if policies.max_iterations != default.max_iterations {
        out.insert("maxIterations".to_owned(), json!(policies.max_iterations));
    }
    if policies.action_error_policy != default.action_error_policy {
        let name = match policies.action_error_policy {
            ActionErrorPolicy::Continue => "continue",
            ActionErrorPolicy::Rollback => "rollback",
            ActionErrorPolicy::Fail => "fail",
        };
        out.insert("actionErrorPolicy".to_owned(), json!(name));
    }
    if policies.guard_error_policy != default.guard_error_policy {
        let value = match policies.guard_error_policy {
            GuardErrorPolicy::False => json!(false),
            GuardErrorPolicy::True => json!(true),
            GuardErrorPolicy::Raise => json!("raise"),
        };
        out.insert("guardErrorPolicy".to_owned(), value);
    }
    if policies.on_unhandled != default.on_unhandled {
        let name = match policies.on_unhandled {
            UnhandledPolicy::Ignore => "ignore",
            UnhandledPolicy::Defer => "defer",
            UnhandledPolicy::Error => "error",
        };
        out.insert("onUnhandled".to_owned(), json!(name));
    }
    out
}

fn state_value(state: &StateConfig) -> Value {
    Value::Object(state_object(state))
}

fn state_object(state: &StateConfig) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(id) = &state.custom_id {
        out.insert("id".to_owned(), json!(id));
    }
    insert_kind(&mut out, &state.kind);
    if let Some(initial) = &state.initial {
        out.insert("initial".to_owned(), json!(initial));
    }
    insert_if_any(&mut out, "entry", state.entry.iter().map(action_value).collect());
    insert_if_any(&mut out, "exit", state.exit.iter().map(action_value).collect());
    insert_invokes(&mut out, &state.invoke);
    insert_handlers(&mut out, state);
    if let Some(output) = &state.output {
        out.insert("output".to_owned(), output.clone());
    }
    if let Some(meta) = &state.meta {
        out.insert("meta".to_owned(), meta.clone());
    }
    if let Some(description) = &state.description {
        out.insert("description".to_owned(), json!(description));
    }
    if !state.tags.is_empty() {
        out.insert("tags".to_owned(), json!(state.tags));
    }
    if !state.states.is_empty() {
        let children: Map<String, Value> =
            state.states.iter().map(|child| (child.key.clone(), state_value(child))).collect();
        out.insert("states".to_owned(), Value::Object(children));
    }
    out
}

fn insert_kind(out: &mut Map<String, Value>, kind: &StateKind) {
    match kind {
        StateKind::Atomic | StateKind::Compound => {}
        StateKind::Parallel => {
            out.insert("type".to_owned(), json!("parallel"));
        }
        StateKind::Final => {
            out.insert("type".to_owned(), json!("final"));
        }
        StateKind::History { deep, target } => {
            out.insert("type".to_owned(), json!("history"));
            out.insert("history".to_owned(), json!(if *deep { "deep" } else { "shallow" }));
            if let Some(target) = target {
                out.insert("target".to_owned(), json!(target));
            }
        }
    }
}

fn insert_handlers(out: &mut Map<String, Value>, state: &StateConfig) {
    if !state.on.is_empty() {
        let on: Map<String, Value> = state
            .on
            .iter()
            .map(|(event, transitions)| (event.clone(), transitions_value(transitions)))
            .collect();
        out.insert("on".to_owned(), Value::Object(on));
    }
    if !state.always.is_empty() {
        out.insert("always".to_owned(), transitions_value(&state.always));
    }
    if !state.after.is_empty() {
        let after: Map<String, Value> = state
            .after
            .iter()
            .map(|(delay, transitions)| (delay.clone(), transitions_value(transitions)))
            .collect();
        out.insert("after".to_owned(), Value::Object(after));
    }
    if !state.on_done.is_empty() {
        out.insert("onDone".to_owned(), transitions_value(&state.on_done));
    }
}

fn insert_invokes(out: &mut Map<String, Value>, invokes: &[InvokeConfig]) {
    insert_if_any(out, "invoke", invokes.iter().map(invoke_value).collect());
}

fn insert_if_any(out: &mut Map<String, Value>, key: &str, items: Vec<Value>) {
    if !items.is_empty() {
        out.insert(key.to_owned(), one_or_many(items));
    }
}

/// A single item is written bare, several as an array (the parser accepts both).
pub(super) fn one_or_many(mut items: Vec<Value>) -> Value {
    if items.len() == 1 { items.remove(0) } else { Value::Array(items) }
}

fn invoke_value(invoke: &InvokeConfig) -> Value {
    let mut out = Map::new();
    if let Some(id) = &invoke.id {
        out.insert("id".to_owned(), json!(id));
    }
    out.insert("src".to_owned(), json!(invoke.src));
    if !invoke.input.is_null() {
        out.insert("input".to_owned(), invoke.input.clone());
    }
    if let Some(system_id) = &invoke.system_id {
        out.insert("systemId".to_owned(), json!(system_id));
    }
    if !invoke.on_done.is_empty() {
        out.insert("onDone".to_owned(), transitions_value(&invoke.on_done));
    }
    if !invoke.on_error.is_empty() {
        out.insert("onError".to_owned(), transitions_value(&invoke.on_error));
    }
    Value::Object(out)
}

fn transitions_value(transitions: &[TransitionConfig]) -> Value {
    one_or_many(transitions.iter().map(transition_value).collect())
}

fn transition_value(t: &TransitionConfig) -> Value {
    if t.forbidden {
        return Value::Null;
    }
    let plain = t.actions.is_empty()
        && t.guard.is_none()
        && !t.reenter
        && t.meta.is_none()
        && t.description.is_none();
    if plain && let [target] = t.targets.as_slice() {
        return json!(target);
    }
    let mut out = Map::new();
    match t.targets.as_slice() {
        [] => {}
        [target] => {
            out.insert("target".to_owned(), json!(target));
        }
        targets => {
            out.insert("target".to_owned(), json!(targets));
        }
    }
    insert_if_any(&mut out, "actions", t.actions.iter().map(action_value).collect());
    if let Some(guard) = &t.guard {
        out.insert("guard".to_owned(), guard_value(guard));
    }
    if t.reenter {
        out.insert("reenter".to_owned(), json!(true));
    }
    if let Some(meta) = &t.meta {
        out.insert("meta".to_owned(), meta.clone());
    }
    if let Some(description) = &t.description {
        out.insert("description".to_owned(), json!(description));
    }
    if out.is_empty() {
        // `{}` would read back as a forbidden transition.
        out.insert("actions".to_owned(), Value::Array(Vec::new()));
    }
    Value::Object(out)
}

/// The JSON spelling of an action: a bare name without params, `{type, params}` otherwise.
pub(super) fn action_value(action: &ActionConfig) -> Value {
    if action.params.is_null() {
        Value::String(action.name.clone())
    } else {
        json!({ "type": action.name, "params": action.params })
    }
}

/// The JSON spelling of a guard (the inverse of the parser's guard grammar).
pub(super) fn guard_value(guard: &GuardConfig) -> Value {
    match guard {
        GuardConfig::Named { name, params } if params.is_null() => Value::String(name.clone()),
        GuardConfig::Named { name, params } => json!({ "type": name, "params": params }),
        GuardConfig::Not(inner) => {
            json!({ "type": "not", "params": { "guard": guard_value(inner) } })
        }
        GuardConfig::And(list) => {
            let guards: Vec<Value> = list.iter().map(guard_value).collect();
            json!({ "type": "and", "params": { "guards": guards } })
        }
        GuardConfig::Or(list) => {
            let guards: Vec<Value> = list.iter().map(guard_value).collect();
            json!({ "type": "or", "params": { "guards": guards } })
        }
    }
}
