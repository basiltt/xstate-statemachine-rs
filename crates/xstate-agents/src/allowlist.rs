//! Per-state tool allow-lists (`meta.tools`, X0.13).
//!
//! Python's services receive the interpreter and read the hosting state's `meta.tools` at call
//! time. The Rust services receive only the context, so the allow-list governing a service
//! is resolved from the chart when the logic is built ([`hosting_tools`]): the hosting
//! state's own `meta.tools`, else the union over its ancestors'; a service hosted by several
//! states gets the INTERSECTION of their lists (never more than any host allows). No
//! `meta.tools` anywhere → `[]`: closed by default.

use serde_json::Value;
use xstate_core::machine::StateNode;
use xstate_core::{Context, Machine, StateId, SyncInterpreter};

use crate::error::{AgentError, Result};
use crate::pyfmt::py_repr_str;
use crate::tools::{ALL_TOOLS, ToolRegistry};

fn meta_tools(node: &StateNode) -> Result<Option<Vec<String>>> {
    let Some(value) = node.meta.as_ref().and_then(|m| m.get("tools")) else { return Ok(None) };
    let list = value.as_array().and_then(|items| {
        items.iter().map(|v| v.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()
    });
    list.map(Some).ok_or_else(|| {
        AgentError::config(format!(
            "state {}: meta.tools must be a list of tool names",
            py_repr_str(&node.id)
        ))
    })
}

/// The union of `meta.tools` over the given active states.
///
/// # Errors
/// [`AgentError::Config`] for a `meta.tools` that is not a list of names.
pub fn tools_of_states(
    machine: &Machine,
    active: impl IntoIterator<Item = StateId>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in active {
        if let Some(list) = meta_tools(machine.node(id))? {
            out.extend(list);
        }
    }
    Ok(out)
}

/// The allow-list governing the interpreter's current step: the union of `meta.tools` over
/// every active state (ancestors included).
///
/// # Errors
/// See [`tools_of_states`].
pub fn state_tools<C: Context>(interp: &SyncInterpreter<C>) -> Result<Vec<String>> {
    let Some(state) = interp.state() else { return Ok(Vec::new()) };
    tools_of_states(interp.machine(), state.configuration())
}

/// States whose `invoke` runs service `src`.
#[must_use]
pub fn hosting_states(machine: &Machine, src: &str) -> Vec<StateId> {
    machine
        .states()
        .iter()
        .filter(|node| node.invokes.iter().any(|inv| inv.src == src))
        .filter_map(|node| machine.state_by_id(&node.id))
        .collect()
}

fn host_tools(machine: &Machine, host: StateId) -> Result<Vec<String>> {
    if let Some(own) = meta_tools(machine.node(host))? {
        return Ok(own);
    }
    let mut out = Vec::new();
    let mut cur = machine.node(host).parent;
    while let Some(p) = cur {
        out.extend(meta_tools(machine.node(p))?.unwrap_or_default());
        cur = machine.node(p).parent;
    }
    Ok(out)
}

/// The allow-list a service sees, resolved from the chart (see the module docs).
///
/// # Errors
/// See [`tools_of_states`].
pub fn hosting_tools(machine: &Machine, src: &str) -> Result<Vec<String>> {
    let lists: Vec<Vec<String>> = hosting_states(machine, src)
        .into_iter()
        .map(|h| host_tools(machine, h))
        .collect::<Result<_>>()?;
    let Some((first, rest)) = lists.split_first() else { return Ok(Vec::new()) };
    if lists.iter().all(|l| l.iter().any(|n| n == ALL_TOOLS)) {
        return Ok(vec![ALL_TOOLS.to_owned()]);
    }
    let mut names: Vec<String> =
        lists.iter().flatten().filter(|n| *n != ALL_TOOLS).cloned().collect();
    names.sort();
    names.dedup();
    let admits = |list: &Vec<String>, n: &String| list.iter().any(|x| x == n || x == ALL_TOOLS);
    names.retain(|n| admits(first, n) && rest.iter().all(|l| admits(l, n)));
    Ok(names)
}

/// The first `meta.<key>` found on a state hosting `src` (or its ancestors).
#[must_use]
pub fn hosting_meta(machine: &Machine, src: &str, key: &str) -> Option<Value> {
    for host in hosting_states(machine, src) {
        let mut cur = Some(host);
        while let Some(s) = cur {
            if let Some(v) = machine
                .node(s)
                .meta
                .as_ref()
                .and_then(|m| m.get(key))
                .filter(|v| crate::pyfmt::truthy(v))
            {
                return Some(v.clone());
            }
            cur = machine.node(s).parent;
        }
    }
    None
}

/// Fail loudly when a state's `meta.tools` names an unregistered tool (a typo would
/// otherwise silently narrow the agent).
///
/// # Errors
/// [`AgentError::Config`] naming the state and the tool.
pub fn validate_agent_chart(machine: &Machine, registry: &ToolRegistry) -> Result<()> {
    for node in machine.states() {
        for name in meta_tools(node)?.unwrap_or_default() {
            if name != ALL_TOOLS && !registry.contains(&name) {
                return Err(AgentError::config(format!(
                    "state {}: meta.tools lists {}, which is not in the tool registry {:?}",
                    py_repr_str(&node.id),
                    py_repr_str(&name),
                    registry.names()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn machine(chart: &Value) -> Machine {
        Machine::from_value(chart).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn hosting_lists_intersect_and_inherit() {
        let m = machine(&json!({
            "id": "m", "initial": "a",
            "states": {
                "a": {"meta": {"tools": ["x", "y"], "output_model": "Out"}, "invoke": {"src": "svc"}},
                "p": {"meta": {"tools": ["*"]}, "initial": "b", "states": {"b": {"invoke": [{"src": "svc"}, {"src": "other"}]}}},
                "c": {"invoke": {"src": "bare"}}
            }
        }));
        assert_eq!(hosting_tools(&m, "svc").unwrap_or_else(|e| panic!("{e}")), ["x", "y"]);
        assert_eq!(hosting_tools(&m, "other").unwrap_or_else(|e| panic!("{e}")), ["*"]);
        assert_eq!(
            hosting_tools(&m, "bare").unwrap_or_else(|e| panic!("{e}")),
            Vec::<String>::new()
        );
        assert_eq!(
            hosting_tools(&m, "missing").unwrap_or_else(|e| panic!("{e}")),
            Vec::<String>::new()
        );
        assert_eq!(hosting_meta(&m, "svc", "output_model"), Some(json!("Out")));
        assert_eq!(hosting_meta(&m, "other", "output_model"), None);
    }

    #[test]
    fn charts_are_validated_against_the_registry() {
        let m = machine(
            &json!({"id": "m", "initial": "a", "states": {"a": {"meta": {"tools": ["typo"]}}}}),
        );
        let reg = ToolRegistry::default();
        let err = validate_agent_chart(&m, &reg).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.starts_with("state 'm.a': meta.tools lists 'typo'"), "{err}");
        let bad =
            machine(&json!({"id": "m", "initial": "a", "states": {"a": {"meta": {"tools": "x"}}}}));
        assert!(validate_agent_chart(&bad, &reg).is_err());
        let mut interp = SyncInterpreter::new(m, xstate_core::MachineLogic::dynamic());
        assert_eq!(state_tools(&interp).unwrap_or_else(|e| panic!("{e}")), Vec::<String>::new());
        interp.start().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(state_tools(&interp).unwrap_or_else(|e| panic!("{e}")), ["typo"]);
    }
}
