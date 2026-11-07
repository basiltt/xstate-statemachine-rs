//! Compilation of a parsed [`MachineConfig`] into the arena-based [`Machine`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use foldhash::HashMap as FastMap;

use serde_json::{Map, Value};
use smallvec::SmallVec;

use super::assemble::{self, Finished};
use super::builtin::{self, ChooseBranch, Delay};
use super::{
    ActionDef, AfterDef, Guard, InvokeDef, MachineData, NodeKind, StateNode, Transition, Trigger,
};
use crate::config::{
    ActionConfig, ConfigWarning, GuardConfig, MAX_STATE_NESTING, MachineConfig, StateConfig,
    StateKind, TransitionConfig,
};
use crate::error::{Error, Result};
use crate::event::names;
use crate::ids::{StateId, TransitionId};

/// Position and naming of one state in the document-order arena, shared by the compiler of
/// parsed configurations and the direct compiler of JSON values (`super::direct`).
pub(super) struct Skel<'c> {
    /// Key under the parent's `states`.
    pub(super) key: &'c str,
    /// Full dotted id.
    pub(super) id: Arc<str>,
    pub(super) parent: Option<StateId>,
    pub(super) depth: u32,
}

/// One config node during compilation.
struct Raw<'r, 'c> {
    cfg: &'c StateConfig,
    skel: &'r Skel<'c>,
    /// Full dotted id (`skel.id`).
    id: &'r str,
    parent: Option<StateId>,
}

pub(super) fn compile(
    config: MachineConfig,
    user_actions: Option<&HashSet<String>>,
) -> Result<MachineData> {
    // A `MachineConfig` can also be assembled by hand (every field is public), so the nesting
    // bound the parser enforces is enforced here as well, before any recursive walk.
    if !nesting_within_bound(&config.root) {
        return Err(Error::LimitExceeded(format!(
            "state nesting exceeds {MAX_STATE_NESTING} levels"
        )));
    }
    let count = count_states(&config.root);
    let mut cfgs: SmallVec<[&StateConfig; 16]> = SmallVec::with_capacity(count);
    let mut skels: SmallVec<[Skel<'_>; 16]> = SmallVec::with_capacity(count);
    let mut id = config.root.key.clone();
    flatten(&config.root, &mut id, None, 0, &mut cfgs, &mut skels);
    let (ids, custom_ids) = assemble::index_ids(&skels, |i| cfgs[i].custom_id.as_deref())?;
    let children = assemble::index_children(&skels);
    let mut cx = Compiler::new(
        &skels,
        (&ids, &children, &custom_ids),
        &config.root.key,
        &config.policies,
        user_actions,
    );
    cx.warnings.clone_from(&config.warnings);
    let mut states: Vec<StateNode> = Vec::with_capacity(skels.len());
    for (i, (cfg, skel)) in cfgs.iter().zip(&skels).enumerate() {
        let raw = Raw { cfg, skel, id: &skel.id, parent: skel.parent };
        states.push(cx.compile_state(StateId::from_index(i), &raw)?);
    }
    let parts = assemble::Parts {
        id,
        context: config.context,
        version: config.version,
        policies: config.policies,
        source: super::source::Source::Document(config.source),
    };
    let finished = finish(cx, states)?;
    Ok(assemble::assemble(finished, ids, custom_ids, parts))
}

/// Complete the compiled states (children, subtree ends, history children, event index) and
/// validate them.
pub(super) fn finish(cx: Compiler<'_, '_>, mut states: Vec<StateNode>) -> Result<Finished> {
    assemble::link_children(cx.skels, &mut states)?;
    cx.validate(&states)?;
    assemble::index_subtree_events(&mut states, cx.event_names.len());
    let Compiler { transitions, warnings, has_always, event_index, event_names, .. } = cx;
    Ok(Finished { states, transitions, warnings, has_always, event_index, event_names })
}

/// Number of states in the subtree of `cfg`, itself included.
fn count_states(cfg: &StateConfig) -> usize {
    1 + cfg.states.iter().map(count_states).sum::<usize>()
}

/// Whether every state in the subtree sits at or below [`MAX_STATE_NESTING`] (root 0).
/// Iterative, so it cannot overflow the stack itself on a hand-built deep configuration.
fn nesting_within_bound(root: &StateConfig) -> bool {
    let mut stack = vec![(root, 0_u32)];
    while let Some((cfg, depth)) = stack.pop() {
        if depth > MAX_STATE_NESTING {
            return false;
        }
        stack.extend(cfg.states.iter().map(|child| (child, depth + 1)));
    }
    true
}

/// Append `cfg` and its subtree in document (pre-)order. `id` holds the node's full id; it is
/// extended in place for the children and restored before returning.
fn flatten<'c>(
    cfg: &'c StateConfig,
    id: &mut String,
    parent: Option<StateId>,
    depth: u32,
    cfgs: &mut SmallVec<[&'c StateConfig; 16]>,
    skels: &mut SmallVec<[Skel<'c>; 16]>,
) {
    let me = StateId::from_index(skels.len());
    cfgs.push(cfg);
    skels.push(Skel { key: &cfg.key, id: Arc::from(id.as_str()), parent, depth });
    let len = id.len();
    for child in &cfg.states {
        id.push('.');
        id.push_str(&child.key);
        flatten(child, id, Some(me), depth + 1, cfgs, skels);
        id.truncate(len);
    }
}

/// Per-state transition tables under construction.
#[derive(Default)]
pub(super) struct Handlers {
    on_exact: SmallVec<[(u32, SmallVec<[TransitionId; 2]>); 4]>,
    on_partial: Vec<(String, SmallVec<[TransitionId; 2]>)>,
    on_wildcard: SmallVec<[TransitionId; 2]>,
    pub(super) always: SmallVec<[TransitionId; 2]>,
    pub(super) after: Vec<AfterDef>,
    /// The timer event type of each `after` entry, interned (shared with its transitions).
    pub(super) after_events: SmallVec<[smol_str::SmolStr; 1]>,
    invokes: Vec<InvokeDef>,
}

/// The id indexes the compiler resolves targets against: full ids, children by key (large
/// machines only), custom ids.
pub(super) type Indexes<'a, 'c> = (
    &'a FastMap<Arc<str>, StateId>,
    &'a FastMap<(StateId, &'c str), StateId>,
    &'a HashMap<String, StateId>,
);

pub(super) struct Compiler<'a, 'c> {
    skels: &'a [Skel<'c>],
    ids: &'a FastMap<Arc<str>, StateId>,
    children: &'a FastMap<(StateId, &'c str), StateId>,
    custom_ids: &'a HashMap<String, StateId>,
    machine_id: &'a str,
    strict_targets: bool,
    strict_config: bool,
    /// Action names the logic implements (user logic wins over a same-named built-in).
    user_actions: Option<&'a HashSet<String>>,
    transitions: Vec<Transition>,
    warnings: Vec<ConfigWarning>,
    /// Interned exact descriptors by key; `event_index` maps them back once there are more
    /// than [`super::EVENT_SCAN_LIMIT`].
    event_names: SmallVec<[smol_str::SmolStr; 4]>,
    event_index: FastMap<smol_str::SmolStr, u32>,
    pub(super) has_always: bool,
}

impl<'a, 'c> Compiler<'a, 'c> {
    pub(super) fn new(
        skels: &'a [Skel<'c>],
        (ids, children, custom_ids): Indexes<'a, 'c>,
        machine_id: &'a str,
        policies: &crate::config::Policies,
        user_actions: Option<&'a HashSet<String>>,
    ) -> Self {
        Self {
            skels,
            ids,
            children,
            custom_ids,
            machine_id,
            strict_targets: policies.strict_targets,
            strict_config: policies.strict_config,
            user_actions,
            transitions: Vec::new(),
            warnings: Vec::new(),
            event_names: SmallVec::new(),
            event_index: FastMap::default(),
            has_always: false,
        }
    }

    /// Reserve room for `n` more transitions.
    pub(super) fn reserve_transitions(&mut self, n: usize) {
        self.transitions.reserve(n);
    }
}

impl Compiler<'_, '_> {
    fn id_of(&self, sid: StateId) -> &str {
        &self.skels[sid.index()].id
    }

    pub(super) fn child_by_key(&self, parent: StateId, key: &str) -> Option<StateId> {
        if !self.children.is_empty() {
            return self.children.get(&(parent, key)).copied();
        }
        // Pre-order: the parent's subtree follows it, deeper than the parent.
        let depth = self.skels.get(parent.index())?.depth;
        self.skels[parent.index() + 1..]
            .iter()
            .take_while(|r| r.depth > depth)
            .position(|r| r.parent == Some(parent) && r.key == key)
            .map(|i| StateId::from_index(parent.index() + 1 + i))
    }

    /// Walk a dotted relative path (`a.b.c`) down from `scope`.
    fn descend(&self, scope: StateId, path: &str) -> Option<StateId> {
        path.split('.').try_fold(scope, |cur, key| self.child_by_key(cur, key))
    }

    /// The key of an exact event descriptor, interning it on first sight.
    pub(super) fn intern_event(&mut self, descriptor: &smol_str::SmolStr) -> Result<u32> {
        let known = if self.event_index.is_empty() {
            self.event_names.iter().position(|n| n == descriptor)
        } else {
            self.event_index.get(descriptor.as_str()).map(|&k| k as usize)
        };
        if let Some(k) = known {
            // Keys are below `u32::MAX` by construction.
            return u32::try_from(k).map_err(|_| Error::LimitExceeded("too many events".into()));
        }
        let next = u32::try_from(self.event_names.len())
            .map_err(|_| Error::LimitExceeded("too many distinct event types".into()))?;
        self.event_names.push(descriptor.clone());
        if self.event_names.len() > super::EVENT_SCAN_LIMIT {
            if self.event_index.is_empty() {
                let names = self.event_names.iter().zip(0..);
                self.event_index = names.map(|(n, k)| (n.clone(), k)).collect();
            } else {
                self.event_index.insert(descriptor.clone(), next);
            }
        }
        Ok(next)
    }

    fn compile_state(&mut self, sid: StateId, raw: &Raw<'_, '_>) -> Result<StateNode> {
        let cfg = raw.cfg;
        let kind = self.compile_kind(sid, raw)?;
        let mut handlers = Handlers::default();
        self.compile_handlers(sid, raw, &kind, &mut handlers)?;
        let initial_key = if matches!(kind, NodeKind::Compound { .. }) {
            cfg.initial.as_deref().map(smol_str::SmolStr::new)
        } else {
            None
        };
        let entry = if cfg.entry.is_empty() {
            Vec::new()
        } else {
            self.compile_actions(sid, &cfg.entry, &format!("{}.entry", raw.id))?
        };
        let exit = if cfg.exit.is_empty() {
            Vec::new()
        } else {
            self.compile_actions(sid, &cfg.exit, &format!("{}.exit", raw.id))?
        };

        let meta = NodeMeta {
            output: cfg.output.clone(),
            meta: cfg.meta.clone(),
            description: cfg.description.clone(),
            tags: cfg.tags.clone(),
        };
        Ok(state_node(raw.skel, kind, handlers, (entry, exit), initial_key, meta))
    }

    fn compile_kind(&mut self, sid: StateId, raw: &Raw<'_, '_>) -> Result<NodeKind> {
        let cfg = raw.cfg;
        Ok(match &cfg.kind {
            StateKind::Atomic => NodeKind::Atomic,
            StateKind::Final => NodeKind::Final,
            StateKind::Parallel => NodeKind::Parallel,
            StateKind::Compound => {
                let key = cfg
                    .initial
                    .as_deref()
                    .ok_or_else(|| Error::config(raw.id, "compound state needs `initial`"))?;
                let initial = self.child_by_key(sid, key).ok_or_else(|| {
                    let children = cfg.states.iter().map(|c| c.key.as_str());
                    let hint = closest_key(key, children)
                        .map_or_else(String::new, |h| format!(" Did you mean '{h}'?"));
                    Error::config(
                        format!("{}.initial", raw.id),
                        format!(
                            "State '{}': 'initial' '{key}' is not one of its child states.{hint}",
                            raw.id
                        ),
                    )
                })?;
                NodeKind::Compound { initial }
            }
            StateKind::History { deep, target } => {
                raw.parent
                    .ok_or_else(|| Error::config(raw.id, "the root cannot be a history state"))?;
                // A history `target` is a sibling path, resolved like a plain target written
                // on the history node itself.
                let target = match target {
                    Some(t) => Some(self.resolve_target(sid, t, raw.id)?),
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
        raw: &Raw<'_, '_>,
        kind: &NodeKind,
        out: &mut Handlers,
    ) -> Result<()> {
        let cfg = raw.cfg;
        for (descriptor, list) in &cfg.on {
            self.add_event(sid, raw, descriptor, list, out)?;
        }
        self.compile_after(sid, raw, out)?;
        if !cfg.on_done.is_empty() {
            if matches!(kind, NodeKind::Atomic | NodeKind::Final | NodeKind::History { .. }) {
                return Err(Error::config(
                    format!("{}.onDone", raw.id),
                    "`onDone` is only valid on compound or parallel states",
                ));
            }
            self.add_event(sid, raw, &names::done_state(raw.id), &cfg.on_done, out)?;
        }
        self.compile_invokes_and_always(sid, raw, out)
    }

    /// Delayed transitions become ordinary transitions on their (normalised) timer event.
    fn compile_after(&mut self, sid: StateId, raw: &Raw<'_, '_>, out: &mut Handlers) -> Result<()> {
        for (raw_key, list) in &raw.cfg.after {
            let (delay, key) = Delay::parse_after_key(raw_key).map_err(|why| {
                Error::config(
                    format!("{}.after.{raw_key}", raw.id),
                    format!(
                        "State '{}': 'after' key '{raw_key}' is not a valid delay ({why}); use a \
                         whole, non-negative number of milliseconds or the name of a delay",
                        raw.id
                    ),
                )
            })?;
            if let Some(clash) = out.after.iter().find(|a| a.key == key) {
                return Err(Error::config(
                    format!("{}.after.{raw_key}", raw.id),
                    format!(
                        "State '{}': 'after' keys '{}' and '{raw_key}' both name the delay '{key}'",
                        raw.id, clash.raw_key
                    ),
                ));
            }
            let event = names::after(&key, raw.id);
            let ty = self.add_event(sid, raw, &event, list, out)?;
            out.after_events.push(ty);
            out.after.push(AfterDef { key, delay, event, raw_key: raw_key.clone() });
        }
        Ok(())
    }

    fn compile_invokes_and_always(
        &mut self,
        sid: StateId,
        raw: &Raw<'_, '_>,
        out: &mut Handlers,
    ) -> Result<()> {
        let cfg = raw.cfg;
        for (i, inv) in cfg.invoke.iter().enumerate() {
            let id = inv.id.clone().unwrap_or_else(|| {
                if i == 0 { raw.id.to_string() } else { format!("{}:{i}", raw.id) }
            });
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
                raw.id,
            )?);
        }
        Ok(())
    }

    fn add_event(
        &mut self,
        sid: StateId,
        raw: &Raw<'_, '_>,
        descriptor: &str,
        list: &[TransitionConfig],
        out: &mut Handlers,
    ) -> Result<smol_str::SmolStr> {
        let descriptor = smol_str::SmolStr::new(names::normalize(descriptor));
        let compiled =
            self.compile_transitions(sid, &Trigger::Event(descriptor.clone()), list, raw.id)?;
        self.file_event(&descriptor, compiled, out)?;
        Ok(descriptor)
    }

    /// File the transitions compiled for `descriptor` under the matching table of `out`.
    pub(super) fn file_event(
        &mut self,
        descriptor: &smol_str::SmolStr,
        compiled: SmallVec<[TransitionId; 2]>,
        out: &mut Handlers,
    ) -> Result<()> {
        if descriptor == names::WILDCARD {
            out.on_wildcard.extend(compiled);
        } else if descriptor.ends_with(".*") {
            match out.on_partial.iter_mut().find(|(d, _)| d == descriptor) {
                Some((_, ts)) => ts.extend(compiled),
                None => out.on_partial.push((descriptor.to_string(), compiled)),
            }
        } else {
            let key = self.intern_event(descriptor)?;
            match out.on_exact.iter_mut().find(|(k, _)| *k == key) {
                Some((_, ts)) => ts.extend(compiled),
                None => out.on_exact.push((key, compiled)),
            }
        }
        Ok(())
    }

    fn compile_transitions(
        &mut self,
        source: StateId,
        trigger: &Trigger,
        list: &[TransitionConfig],
        path: &str,
    ) -> Result<SmallVec<[TransitionId; 2]>> {
        let mut out = SmallVec::new();
        for t in list {
            let targets = self.resolve_targets(source, &t.targets, path)?;
            let actions = self.compile_actions(source, &t.actions, path)?;
            let guard = match &t.guard {
                Some(g) => Some(self.compile_guard(source, g, path)?),
                None => None,
            };
            out.push(self.push_transition(Transition {
                source,
                trigger: trigger.clone(),
                targets,
                raw_targets: t.targets.iter().map(smol_str::SmolStr::new).collect(),
                actions,
                guard,
                reenter: t.reenter,
                forbidden: t.forbidden,
                meta: t.meta.clone(),
                description: t.description.clone(),
            }));
        }
        Ok(out)
    }

    /// Resolve the targets written on `source`; the root is never a target.
    pub(super) fn resolve_targets<T: AsRef<str>>(
        &mut self,
        source: StateId,
        written: &[T],
        path: &str,
    ) -> Result<Vec<StateId>> {
        let mut targets = Vec::with_capacity(written.len());
        for target in written {
            let resolved = self.resolve_target(source, target.as_ref(), path)?;
            if resolved == StateId::ROOT {
                return Err(Error::RootTarget { from: self.id_of(source).to_owned() });
            }
            targets.push(resolved);
        }
        Ok(targets)
    }

    /// Append a compiled transition.
    pub(super) fn push_transition(&mut self, transition: Transition) -> TransitionId {
        let tid = TransitionId::from_index(self.transitions.len());
        self.transitions.push(transition);
        tid
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
        let user = self.user_actions.is_some_and(|names| names.contains(&action.name));
        let Some(canonical) = builtin::canonical(&action.name).filter(|_| !user) else {
            return Ok(ActionDef::User {
                name: Arc::from(action.name.as_str()),
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
                let strict = self.strict_config;
                let guard = match obj.get("guard").or_else(|| obj.get("cond")) {
                    None => None,
                    Some(g) => {
                        let parsed =
                            crate::config::parse_guard_value(g, &bp, strict, &mut self.warnings)?;
                        Some(self.compile_guard(source, &parsed, &bp)?)
                    }
                };
                let parsed = crate::config::parse_actions_value(
                    obj.get("actions"),
                    &bp,
                    strict,
                    &mut self.warnings,
                )?;
                let actions = self.compile_actions(source, &parsed, &bp)?;
                out.push(ChooseBranch { guard, actions });
            }
            return Ok(ActionDef::Builtin {
                name: Arc::from(action.name.as_str()),
                builtin: builtin::Builtin::Choose(out),
                params: action.params.clone(),
            });
        }
        Ok(ActionDef::Builtin {
            name: Arc::from(action.name.as_str()),
            builtin: builtin::compile_builtin(canonical, &action.params, path)?,
            params: action.params.clone(),
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
            return resolve_hash_id(self.ids, self.custom_ids, rest, |scope, path| {
                self.descend(scope, path)
            })
            .ok_or_else(not_found);
        }
        if let Some(rest) = target.strip_prefix('.') {
            if rest.is_empty() {
                return Err(Error::config(path, "`.` alone is not a valid target"));
            }
            if let Some(sid) = self.descend(source, rest) {
                return Ok(sid);
            }
            if let Some(parent) = self.skels[source.index()].parent
                && let Some(sid) = self.descend(parent, rest)
            {
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
            return Err(not_found());
        }
        // Plain path: XState resolves it against the parent; the Python dialect also lets it
        // bubble up through further ancestors. The root has no parent, so its own children
        // are the first scope.
        let mut scope = self.skels[source.index()].parent.or(Some(StateId::ROOT));
        while let Some(s) = scope {
            if let Some(sid) = self.descend(s, target) {
                return Ok(sid);
            }
            scope = self.skels[s.index()].parent;
        }
        if target == self.machine_id {
            return Ok(StateId::ROOT);
        }
        Err(not_found())
    }

    fn validate(&self, states: &[StateNode]) -> Result<()> {
        for node in states {
            if let NodeKind::History { .. } = node.kind {
                let parent = node.parent.map(|p| &states[p.index()]);
                if !parent.is_some_and(|p| p.is_compound() || p.is_parallel()) {
                    return Err(Error::config(
                        &*node.id,
                        "history states must be children of a compound or parallel state",
                    ));
                }
            }
            if node.is_final() && !node.children.is_empty() {
                return Err(Error::config(&*node.id, "final states cannot have children"));
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
                    &*states[t.source.index()].id,
                    "unguarded `always` self-transition can never make progress",
                ));
            }
        }
        Ok(())
    }
}

/// A state's descriptive parts.
#[derive(Default)]
pub(super) struct NodeMeta {
    pub(super) output: Option<Value>,
    pub(super) meta: Option<Value>,
    pub(super) description: Option<String>,
    pub(super) tags: Vec<String>,
}

/// A compiled state from its parts.
pub(super) fn state_node(
    skel: &Skel<'_>,
    kind: NodeKind,
    handlers: Handlers,
    (entry, exit): (Vec<ActionDef>, Vec<ActionDef>),
    initial_key: Option<smol_str::SmolStr>,
    NodeMeta { output, meta, description, tags }: NodeMeta,
) -> StateNode {
    let Handlers { on_exact, mut on_partial, on_wildcard, always, after, after_events, invokes } =
        handlers;
    on_partial.sort_by_key(|(d, _)| std::cmp::Reverse(d.len()));
    StateNode {
        id: Arc::clone(&skel.id),
        key: smol_str::SmolStr::new(skel.key),
        parent: skel.parent,
        children: Vec::new(),
        depth: skel.depth,
        kind,
        subtree_end: 0,
        on_exact,
        on_partial,
        on_wildcard,
        always,
        entry,
        exit,
        invokes,
        after_events,
        after,
        initial_key,
        output,
        meta,
        description,
        tags,
        history_children: SmallVec::new(),
        subtree_events: crate::engine::bits::StateBits::default(),
        subtree_wildcards: false,
    }
}

/// Resolve the part of a `#id` reference after the `#`: a full state id, a custom id, or a
/// custom id followed by a relative path (`#my.id.child`). Ids may themselves contain dots, so
/// the longest dotted prefix naming a custom id wins, as in the Python library.
pub(super) fn resolve_hash_id(
    ids: &FastMap<Arc<str>, StateId>,
    custom_ids: &HashMap<String, StateId>,
    reference: &str,
    descend: impl Fn(StateId, &str) -> Option<StateId>,
) -> Option<StateId> {
    if let Some(&sid) = custom_ids.get(reference).or_else(|| ids.get(reference)) {
        return Some(sid);
    }
    let mut cut = reference.len();
    while let Some(dot) = reference[..cut].rfind('.') {
        if let Some(&scope) = custom_ids.get(&reference[..dot])
            && let Some(sid) = descend(scope, &reference[dot + 1..])
        {
            return Some(sid);
        }
        cut = dot;
    }
    None
}

/// Closest candidate by edit distance, for "did you mean" hints.
fn closest_key<'a>(key: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|c| (edit_distance(key, c), c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}
