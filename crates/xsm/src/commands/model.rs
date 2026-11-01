//! Row builders shared by `inspect` and `docs`: transitions, policies, kinds, short ids.

use xstate_core::Machine;
use xstate_core::config::{ActionErrorPolicy, GuardErrorPolicy, UnhandledPolicy};
use xstate_core::machine::{ActionDef, Guard, NodeKind, Transition, Trigger};

/// Name of a node kind as shown to users.
pub(crate) fn kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Atomic => "atomic",
        NodeKind::Compound { .. } => "compound",
        NodeKind::Parallel => "parallel",
        NodeKind::Final => "final",
        NodeKind::History { .. } => "history",
    }
}

/// A state id without the machine-id prefix (`toggle.on` becomes `on`); the root keeps its id.
pub(crate) fn short_id<'a>(machine: &Machine, id: &'a str) -> &'a str {
    id.strip_prefix(machine.id()).and_then(|rest| rest.strip_prefix('.')).unwrap_or(id)
}

/// A guard expression as text: `isAdult`, `stateIn(a.b)`, `not(x)`, `and(a, b)`.
pub(crate) fn guard_text(guard: &Guard) -> String {
    match guard {
        Guard::Named { name, .. } => name.clone(),
        Guard::StateIn { raw, .. } => format!("stateIn({raw})"),
        Guard::Not(inner) => format!("not({})", guard_text(inner)),
        Guard::And(list) => format!("and({})", join_guards(list)),
        Guard::Or(list) => format!("or({})", join_guards(list)),
    }
}

fn join_guards(list: &[Guard]) -> String {
    list.iter().map(guard_text).collect::<Vec<_>>().join(", ")
}

/// An action reference as text: the registry name or the built-in's spelling.
pub(crate) fn action_text(action: &ActionDef) -> String {
    match action {
        ActionDef::User { name, .. } | ActionDef::Builtin { name, .. } => name.to_string(),
    }
}

/// Comma-joined action names.
pub(crate) fn action_list(actions: &[ActionDef]) -> String {
    actions.iter().map(action_text).collect::<Vec<_>>().join(", ")
}

/// What triggers a transition, in the words of the configuration (`GO`, `after 500`, `always`).
pub(crate) fn trigger_label(machine: &Machine, transition: &Transition) -> String {
    let Trigger::Event(name) = &transition.trigger else { return "always".to_owned() };
    let source = &machine.node(transition.source).id;
    if let Some(rest) = name.strip_prefix("xstate.after.") {
        let key = rest.strip_suffix(&format!(".{source}")).unwrap_or(rest);
        return format!("after {key}");
    }
    if name.starts_with("xstate.done.state.") {
        return "onDone".to_owned();
    }
    if let Some(id) = name.strip_prefix("xstate.done.actor.") {
        return format!("invoke {id} onDone");
    }
    if let Some(id) = name.strip_prefix("xstate.error.actor.") {
        return format!("invoke {id} onError");
    }
    name.to_string()
}

/// One row of the transitions table.
#[derive(Debug)]
pub(crate) struct TransitionRow {
    /// Trigger label.
    pub(crate) event: String,
    /// Full id of the owning state.
    pub(crate) from: String,
    /// Full ids of the targets, comma-joined; `(internal)` when targetless.
    pub(crate) to: String,
    /// Guard text, empty when unguarded.
    pub(crate) guard: String,
    /// Comma-joined action names.
    pub(crate) actions: String,
}

/// Every transition of the machine in document order.
pub(crate) fn transition_rows(machine: &Machine) -> Vec<TransitionRow> {
    machine
        .transitions()
        .iter()
        .map(|t| {
            let to = if t.forbidden {
                "(forbidden)".to_owned()
            } else if t.targets.is_empty() {
                "(internal)".to_owned()
            } else {
                t.targets
                    .iter()
                    .map(|id| machine.node(*id).id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            TransitionRow {
                event: trigger_label(machine, t),
                from: machine.node(t.source).id.to_string(),
                to,
                guard: t.guard.as_ref().map(guard_text).unwrap_or_default(),
                actions: action_list(&t.actions),
            }
        })
        .collect()
}

/// Runtime policies as `(name, value)` rows, in the names the configuration uses.
pub(crate) fn policy_rows(machine: &Machine) -> Vec<(&'static str, String)> {
    let p = machine.policies();
    let action_error = match p.action_error_policy {
        ActionErrorPolicy::Continue => "continue",
        ActionErrorPolicy::Rollback => "rollback",
        ActionErrorPolicy::Fail => "fail",
    };
    let guard_error = match p.guard_error_policy {
        GuardErrorPolicy::False => "false",
        GuardErrorPolicy::True => "true",
        GuardErrorPolicy::Raise => "raise",
    };
    let unhandled = match p.on_unhandled {
        UnhandledPolicy::Ignore => "ignore",
        UnhandledPolicy::Defer => "defer",
        UnhandledPolicy::Error => "error",
    };
    vec![
        ("actionErrorPolicy", action_error.to_owned()),
        ("guardErrorPolicy", guard_error.to_owned()),
        ("onUnhandled", unhandled.to_owned()),
        ("maxIterations", p.max_iterations.to_string()),
        ("strict", p.strict.to_string()),
        ("strictTargets", p.strict_targets.to_string()),
        ("strictConfig", p.strict_config.to_string()),
    ]
}

/// File-name-safe form of a machine id: path separators and anything exotic become `_`.
pub(crate) fn file_stem_for(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_start_matches('.');
    if trimmed.is_empty() { "machine".to_owned() } else { trimmed.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_neutralises_path_tricks() {
        assert_eq!(file_stem_for("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(file_stem_for("order flow"), "order_flow");
        assert_eq!(file_stem_for("..."), "machine");
        assert_eq!(file_stem_for("toggle"), "toggle");
    }
}
