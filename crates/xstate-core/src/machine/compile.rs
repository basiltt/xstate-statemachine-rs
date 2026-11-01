//! Compilation of a parsed [`MachineConfig`] into the arena-based [`Machine`].

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use smallvec::SmallVec;

use super::builtin::{self, ChooseBranch, Delay};
use super::{
    ActionDef, AfterDef, Guard, InvokeDef, MachineData, NodeKind, StateNode, Transition, Trigger,
};
use crate::config::{
    ActionConfig, ConfigWarning, GuardConfig, MachineConfig, StateConfig, StateKind,
    TransitionConfig,
};
use crate::error::{Error, Result};
use crate::event::names;
use crate::ids::{StateId, TransitionId};

/// Flattened view of one config node during compilation.
struct Raw<'c> {
    cfg: &'c StateConfig,
    id: String,
    parent: Option<StateId>,
    depth: u32,
}

pub(super) fn compile(config: MachineConfig) -> Result<MachineData> {
    let mut raws: Vec<Raw<'_>> = Vec::new();
    flatten(&config.root, &config.root.key, None, 0, &mut raws);

    let mut ids: HashMap<String, StateId> = HashMap::with_capacity(raws.len());
    let mut custom_ids: HashMap<String, StateId> = HashMap::new();
    for (i, raw) in raws.iter().enumerate() {
        let sid = StateId::from_index(i);
        ids.insert(raw.id.clone(), sid);
        if let Some(custom) = &raw.cfg.custom_id
            && custom_ids.insert(custom.clone(), sid).is_some()
        {
            return Err(Error::config(&raw.id, format!("duplicate custom id `#{custom}`")));
        }
    }

    let mut cx = Compiler {
        raws: &raws,
        ids: &ids,
        custom_ids: &custom_ids,
        machine_id: &config.root.key,
        strict_targets: config.policies.strict_targets,
        transitions: Vec::new(),
        warnings: config.warnings.clone(),
        declared_events: HashSet::new(),
        has_always: false,
    };

    let mut states: Vec<StateNode> = Vec::with_capacity(raws.len());
    for (i, raw) in raws.iter().enumerate() {
        let sid = StateId::from_index(i);
        states.push(cx.compile_state(sid, raw)?);
    }

    // Children, subtree ends and history children from the flattened order.
    for i in (0..raws.len()).rev() {
        let sid = StateId::from_index(i);
        let mut end = i + 1;
        for (j, r) in raws.iter().enumerate().skip(i + 1) {
            if r.depth <= raws[i].depth {
                break;
            }
            end = j + 1;
        }
        states[i].subtree_end =
            u32::try_from(end).map_err(|_| Error::LimitExceeded("too many states".into()))?;
        if let Some(parent) = raws[i].parent {
            states[parent.index()].children.insert(0, sid);
            if states[i].is_history() {
                states[parent.index()].history_children.insert(0, sid);
            }
        }
    }

    cx.validate(&states)?;

    let Compiler { transitions, warnings, declared_events, has_always, .. } = cx;
    let mut declared: Vec<String> = declared_events.into_iter().collect();
    declared.sort();
    Ok(MachineData {
        id: config.root.key.clone(),
        states,
        transitions,
        ids,
        custom_ids,
        context: config.context,
        version: config.version,
        policies: config.policies,
        warnings,
        has_always,
        declared_events: declared,
        hash: std::sync::OnceLock::new(),
    })
}

fn flatten<'c>(
    cfg: &'c StateConfig,
    id: &str,
    parent: Option<StateId>,
    depth: u32,
    out: &mut Vec<Raw<'c>>,
) {
    let my_index = out.len();
    out.push(Raw { cfg, id: id.to_owned(), parent, depth });
    let me = StateId::from_index(my_index);
    for child in &cfg.states {
        let child_id = format!("{id}.{}", child.key);
        flatten(child, &child_id, Some(me), depth + 1, out);
    }
}

/// Per-state transition tables under construction.
#[derive(Default)]
struct Handlers {
    on_exact: HashMap<String, SmallVec<[TransitionId; 2]>>,
    on_partial: Vec<(String, SmallVec<[TransitionId; 2]>)>,
    on_wildcard: SmallVec<[TransitionId; 2]>,
    always: SmallVec<[TransitionId; 2]>,
    after: Vec<AfterDef>,
    invokes: Vec<InvokeDef>,
}

struct Compiler<'a, 'c> {
    raws: &'a [Raw<'c>],
    ids: &'a HashMap<String, StateId>,
    custom_ids: &'a HashMap<String, StateId>,
    machine_id: &'a str,
    strict_targets: bool,
    transitions: Vec<Transition>,
    warnings: Vec<ConfigWarning>,
    declared_events: HashSet<String>,
    has_always: bool,
}

impl Compiler<'_, '_> {
    fn id_of(&self, sid: StateId) -> &str {
        &self.raws[sid.index()].id
    }

    fn child_by_key(&self, parent: StateId, key: &str) -> Option<StateId> {
        self.ids.get(&format!("{}.{key}", self.id_of(parent))).copied()
    }

    fn compile_state(&mut self, sid: StateId, raw: &Raw<'_>) -> Result<StateNode> {
        let cfg = raw.cfg;
        let kind = self.compile_kind(sid, raw)?;
        let mut handlers = Handlers::default();
        self.compile_handlers(sid, raw, &kind, &mut handlers)?;
        let Handlers { on_exact, mut on_partial, on_wildcard, always, after, invokes } = handlers;
        on_partial.sort_by_key(|(d, _)| std::cmp::Reverse(d.len()));

        let initial_key =
            if matches!(kind, NodeKind::Compound { .. }) { cfg.initial.clone() } else { None };
        let entry = self.compile_actions(sid, &cfg.entry, &format!("{}.entry", raw.id))?;
        let exit = self.compile_actions(sid, &cfg.exit, &format!("{}.exit", raw.id))?;

        Ok(StateNode {
            id: raw.id.clone(),
            key: cfg.key.clone(),
            parent: raw.parent,
            children: Vec::new(),
            depth: raw.depth,
            kind,
            subtree_end: 0,
            on_exact,
            on_partial,
            on_wildcard,
            always,
            entry,
            exit,
            invokes,
            after,
            initial_key,
            output: cfg.output.clone(),
            meta: cfg.meta.clone(),
            description: cfg.description.clone(),
            tags: cfg.tags.clone(),
            history_children: SmallVec::new(),
        })
    }

    fn compile_kind(&mut self, sid: StateId, raw: &Raw<'_>) -> Result<NodeKind> {
        let cfg = raw.cfg;
        Ok(match &cfg.kind {
            StateKind::Atomic => NodeKind::Atomic,
            StateKind::Final => NodeKind::Final,
            StateKind::Parallel => NodeKind::Parallel,
            StateKind::Compound => {
                let key = cfg
                    .initial
                    .as_deref()
                    .ok_or_else(|| Error::config(&raw.id, "compound state needs `initial`"))?;
                let initial = self.child_by_key(sid, key).ok_or_else(|| {
                    Error::config(
                        format!("{}.initial", raw.id),
                        format!("`initial` names unknown child `{key}`"),
                    )
                })?;
                NodeKind::Compound { initial }
            }
            StateKind::History { deep, target } => {
                raw.parent
                    .ok_or_else(|| Error::config(&raw.id, "the root cannot be a history state"))?;
                // A history `target` is a sibling path, resolved like a plain target written
                // on the history node itself.
                let target = match target {
                    Some(t) => Some(self.resolve_target(sid, t, &raw.id)?),
                    None => None,
                };
                NodeKind::History { deep: *deep, target }
            }
        })
    }

    /// Compile `on`, `after`, `onDone`, invoke completions and `always` into transitions.
    fn compile_handlers(
        &mut self,
        sid: StateId,
        raw: &Raw<'_>,
        kind: &NodeKind,
        out: &mut Handlers,
    ) -> Result<()> {
        let cfg = raw.cfg;
        for (descriptor, list) in &cfg.on {
            self.add_event(sid, raw, descriptor, list, out)?;
        }
        // Delayed transitions become ordinary transitions on their timer event.
        for (key, list) in &cfg.after {
            let delay = Delay::parse(&Value::String(key.clone())).ok_or_else(|| {
                Error::config(format!("{}.after.{key}", raw.id), "invalid delay key")
            })?;
            let event = names::after(key, &raw.id);
            self.add_event(sid, raw, &event, list, out)?;
            out.after.push(AfterDef { key: key.clone(), delay, event });
        }
        if !cfg.on_done.is_empty() {
            if matches!(kind, NodeKind::Atomic | NodeKind::Final | NodeKind::History { .. }) {
                return Err(Error::config(
                    format!("{}.onDone", raw.id),
                    "`onDone` is only valid on compound or parallel states",
                ));
            }
            self.add_event(sid, raw, &names::done_state(&raw.id), &cfg.on_done, out)?;
        }
        for (i, inv) in cfg.invoke.iter().enumerate() {
            let id = inv
                .id
                .clone()
                .unwrap_or_else(|| if i == 0 { raw.id.clone() } else { format!("{}:{i}", raw.id) });
            self.add_event(sid, raw, &names::done_actor(&id), &inv.on_done, out)?;
            self.add_event(sid, raw, &names::error_actor(&id), &inv.on_error, out)?;
            out.invokes.push(InvokeDef {
                id,
                src: inv.src.clone(),
                input: inv.input.clone(),
                system_id: inv.system_id.clone(),
            });
        }
        if !cfg.always.is_empty() {
            self.has_always = true;
            out.always.extend(self.compile_transitions(
                sid,
                &Trigger::Always,
                &cfg.always,
                &raw.id,
            )?);
        }
        Ok(())
    }

    fn add_event(
        &mut self,
        sid: StateId,
        raw: &Raw<'_>,
        descriptor: &str,
        list: &[TransitionConfig],
        out: &mut Handlers,
    ) -> Result<()> {
        let descriptor = names::normalize(descriptor).into_owned();
        let compiled =
            self.compile_transitions(sid, &Trigger::Event(descriptor.clone()), list, &raw.id)?;
        if descriptor == names::WILDCARD {
            out.on_wildcard.extend(compiled);
        } else if descriptor.ends_with(".*") {
            match out.on_partial.iter_mut().find(|(d, _)| *d == descriptor) {
                Some((_, ts)) => ts.extend(compiled),
                None => out.on_partial.push((descriptor, compiled.into_iter().collect())),
            }
        } else {
            if !descriptor.starts_with("xstate.") {
                self.declared_events.insert(descriptor.clone());
            }
            out.on_exact.entry(descriptor).or_default().extend(compiled);
        }
        Ok(())
    }

    fn compile_transitions(
        &mut self,
        source: StateId,
        trigger: &Trigger,
        list: &[TransitionConfig],
        path: &str,
    ) -> Result<Vec<TransitionId>> {
        let mut out = Vec::with_capacity(list.len());
        for t in list {
            let mut targets = Vec::with_capacity(t.targets.len());
            for target in &t.targets {
                let resolved = self.resolve_target(source, target, path)?;
                if resolved == StateId::ROOT {
                    return Err(Error::RootTarget { from: self.id_of(source).to_owned() });
                }
                targets.push(resolved);
            }
            let actions = self.compile_actions(source, &t.actions, path)?;
            let guard = match &t.guard {
                Some(g) => Some(self.compile_guard(source, g, path)?),
                None => None,
            };
            let tid = TransitionId::from_index(self.transitions.len());
            self.transitions.push(Transition {
                source,
                trigger: trigger.clone(),
                targets,
                raw_targets: t.targets.clone(),
                actions,
                guard,
                reenter: t.reenter,
                forbidden: t.forbidden,
                meta: t.meta.clone(),
                description: t.description.clone(),
            });
            out.push(tid);
        }
        Ok(out)
    }

    fn compile_actions(
        &mut self,
        source: StateId,
        list: &[ActionConfig],
        path: &str,
    ) -> Result<Vec<ActionDef>> {
        list.iter().map(|a| self.compile_action(source, a, path)).collect()
    }

    fn compile_action(
        &mut self,
        source: StateId,
        action: &ActionConfig,
        path: &str,
    ) -> Result<ActionDef> {
        let Some(canonical) = builtin::canonical(&action.name) else {
            return Ok(ActionDef::User {
                name: action.name.clone(),
                params: action.params.clone(),
            });
        };
        if canonical == "choose" {
            let empty = Map::new();
            let params = action.params.as_object().unwrap_or(&empty);
            let branches = params.get("branches").and_then(Value::as_array).ok_or_else(|| {
                Error::config(path, "built-in `choose` requires `params.branches` (array)")
            })?;
            let mut out = Vec::with_capacity(branches.len());
            for (i, b) in branches.iter().enumerate() {
                let bp = format!("{path}.branches[{i}]");
                let obj =
                    b.as_object().ok_or_else(|| Error::config(&bp, "branch must be an object"))?;
                let guard = match obj.get("guard").or_else(|| obj.get("cond")) {
                    None => None,
                    Some(g) => Some(self.compile_guard(
                        source,
                        &crate::config::parse_guard_value(g, &bp)?,
                        &bp,
                    )?),
                };
                let actions = self.compile_actions(
                    source,
                    &crate::config::parse_actions_value(obj.get("actions"), &bp)?,
                    &bp,
                )?;
                out.push(ChooseBranch { guard, actions });
            }
            return Ok(ActionDef::Builtin {
                name: action.name.clone(),
                builtin: builtin::Builtin::Choose(out),
            });
        }
        Ok(ActionDef::Builtin {
            name: action.name.clone(),
            builtin: builtin::compile_builtin(canonical, &action.params, path)?,
        })
    }

    fn compile_guard(&mut self, source: StateId, guard: &GuardConfig, path: &str) -> Result<Guard> {
        Ok(match guard {
            GuardConfig::Named { name, params } if name == "stateIn" => {
                let target = match params {
                    Value::String(s) => s.as_str(),
                    Value::Object(m) => m
                        .get("stateValue")
                        .or_else(|| m.get("state"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::config(path, "`stateIn` needs a state id string"))?,
                    _ => return Err(Error::config(path, "`stateIn` needs a state id string")),
                };
                Guard::StateIn {
                    state: self.resolve_target(source, target, path)?,
                    raw: target.to_owned(),
                }
            }
            GuardConfig::Named { name, params } => {
                Guard::Named { name: name.clone(), params: params.clone() }
            }
            GuardConfig::Not(inner) => {
                Guard::Not(Box::new(self.compile_guard(source, inner, path)?))
            }
            GuardConfig::And(list) => Guard::And(
                list.iter().map(|g| self.compile_guard(source, g, path)).collect::<Result<_>>()?,
            ),
            GuardConfig::Or(list) => Guard::Or(
                list.iter().map(|g| self.compile_guard(source, g, path)).collect::<Result<_>>()?,
            ),
        })
    }

    /// Resolve a target string written on `source`.
    fn resolve_target(&mut self, source: StateId, target: &str, path: &str) -> Result<StateId> {
        let not_found =
            || Error::StateNotFound { target: target.to_owned(), from: path.to_owned() };
        if target.is_empty() {
            return Err(Error::config(path, "empty target"));
        }
        if let Some(rest) = target.strip_prefix('#') {
            if let Some(&sid) = self.custom_ids.get(rest) {
                return Ok(sid);
            }
            return self.ids.get(rest).copied().ok_or_else(not_found);
        }
        if let Some(rest) = target.strip_prefix('.') {
            if rest.is_empty() {
                return Err(Error::config(path, "`.` alone is not a valid target"));
            }
            let own = format!("{}.{rest}", self.id_of(source));
            if let Some(&sid) = self.ids.get(&own) {
                return Ok(sid);
            }
            if let Some(parent) = self.raws[source.index()].parent {
                let sibling = format!("{}.{rest}", self.id_of(parent));
                if let Some(&sid) = self.ids.get(&sibling) {
                    if self.strict_targets {
                        return Err(Error::config(
                            path,
                            format!(
                                "`.{rest}` is not a child of `{}`; write `{rest}` for the sibling",
                                self.id_of(source)
                            ),
                        ));
                    }
                    self.warnings.push(ConfigWarning {
                        path: path.to_owned(),
                        message: format!(
                            "`.{rest}` resolved to a sibling; this fallback is deprecated"
                        ),
                    });
                    return Ok(sid);
                }
            }
            return Err(not_found());
        }
        // Plain path: XState resolves it against the parent; the Python dialect also lets it
        // bubble up through further ancestors. The root has no parent, so its own children
        // are the first scope.
        let mut scope = self.raws[source.index()].parent.or(Some(StateId::ROOT));
        while let Some(s) = scope {
            if let Some(&sid) = self.ids.get(&format!("{}.{target}", self.id_of(s))) {
                return Ok(sid);
            }
            scope = self.raws[s.index()].parent;
        }
        if target == self.machine_id {
            return Ok(StateId::ROOT);
        }
        Err(not_found())
    }

    fn validate(&mut self, states: &[StateNode]) -> Result<()> {
        for node in states {
            if let NodeKind::History { .. } = node.kind {
                let parent = node.parent.map(|p| &states[p.index()]);
                if !parent.is_some_and(|p| p.is_compound() || p.is_parallel()) {
                    return Err(Error::config(
                        &node.id,
                        "history states must be children of a compound or parallel state",
                    ));
                }
            }
            if node.is_final() && !node.children.is_empty() {
                return Err(Error::config(&node.id, "final states cannot have children"));
            }
        }
        // A dead `always` self-loop (no target change, no actions) would spin forever.
        for t in &self.transitions {
            if t.trigger == Trigger::Always
                && t.guard.is_none()
                && t.actions.is_empty()
                && !t.reenter
                && t.targets.iter().all(|&x| x == t.source)
                && !t.targets.is_empty()
            {
                return Err(Error::config(
                    &states[t.source.index()].id,
                    "unguarded `always` self-transition can never make progress",
                ));
            }
        }
        Ok(())
    }
}
