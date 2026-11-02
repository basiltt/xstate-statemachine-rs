//! Compile-time structural checks of a parsed chart: unique custom ids, `initial` naming a
//! child, kinds that fit the tree shape, and every target resolving the way the runtime
//! resolver (`xstate_core::machine`) will resolve it.

use std::collections::HashMap;

use super::ast::{Body, Chart, Kind, Name, Target, TargetKind, Transition};

/// One state of the flattened tree.
struct Node<'a> {
    id: String,
    name: &'a Name,
    parent: Option<usize>,
    children: Vec<usize>,
    body: &'a Body,
}

/// The flattened tree plus the lookup tables of the runtime resolver.
struct Index<'a> {
    nodes: Vec<Node<'a>>,
    by_id: HashMap<String, usize>,
    custom: HashMap<&'a str, usize>,
    strict_targets: bool,
}

/// Run every check; all problems are reported together.
pub(crate) fn validate(chart: &Chart) -> syn::Result<()> {
    let mut errors = Vec::new();
    let mut index = Index {
        nodes: Vec::new(),
        by_id: HashMap::new(),
        custom: HashMap::new(),
        strict_targets: !(chart.root.root_keys.iter())
            .any(|(k, v)| k == "strictTargets" && v.to_string() == "false"),
    };
    index.add(&chart.id, chart.id.value.clone(), None, &chart.root, &mut errors);
    for i in 0..index.nodes.len() {
        index.check_shape(i, &mut errors);
        index.check_targets(i, &mut errors);
    }
    let mut errors = errors.into_iter();
    let Some(mut first) = errors.next() else { return Ok(()) };
    errors.for_each(|e| first.combine(e));
    Err(first)
}

impl<'a> Index<'a> {
    fn add(
        &mut self,
        name: &'a Name,
        id: String,
        parent: Option<usize>,
        body: &'a Body,
        errors: &mut Vec<syn::Error>,
    ) -> usize {
        let me = self.nodes.len();
        self.by_id.insert(id.clone(), me);
        if let Some(custom) = &body.id
            && self.custom.insert(&custom.value, me).is_some()
        {
            errors.push(custom.error(format!("duplicate state id `{}`", custom.value)));
        }
        self.nodes.push(Node { id, name, parent, children: Vec::new(), body });
        for child in body.states.iter().flatten() {
            let child_id = format!("{}.{}", self.nodes[me].id, child.name.value);
            let c = self.add(&child.name, child_id, Some(me), &child.body, errors);
            self.nodes[me].children.push(c);
        }
        me
    }

    /// Kind, `initial`, `onDone` and history `target` against the node's children.
    fn check_shape(&self, i: usize, errors: &mut Vec<syn::Error>) {
        let node = &self.nodes[i];
        let body = node.body;
        let kind = body.kind.map(|(k, _)| k);
        let has_children = !node.children.is_empty();
        if let Some((k @ (Kind::Final | Kind::History(_) | Kind::Atomic), span)) = body.kind
            && has_children
        {
            let what = if k == Kind::Final { "final" } else { "history or atomic" };
            errors.push(syn::Error::new(span, format!("{what} states cannot have children")));
        }
        if let Some(initial) = &body.initial {
            if !has_children {
                errors.push(initial.error(format!("`{}` has no child states", node.id)));
            } else if self.child(i, &initial.value).is_none() {
                let children = self.child_names(i);
                let msg = format!("`initial` must name a child of `{}` ({children})", node.id);
                errors.push(initial.error(msg));
            }
        } else if kind != Some(Kind::Parallel) && self.needs_initial(i) {
            let msg = format!("`{}` has several children and needs `initial`", node.id);
            errors.push(node.name.error(msg));
        }
        if let Some((span, _)) = &body.on_done
            && (!has_children || matches!(kind, Some(Kind::Final | Kind::History(_))))
        {
            let msg = "`onDone` is only valid on compound or parallel states";
            errors.push(syn::Error::new(*span, msg));
        }
        if let Some((span, _)) = &body.target
            && !matches!(kind, Some(Kind::History(_)))
        {
            errors.push(syn::Error::new(*span, "`target` is only valid with `type: history`"));
        }
    }

    fn needs_initial(&self, i: usize) -> bool {
        let regular = (self.nodes[i].children.iter())
            .filter(|&&c| !matches!(self.nodes[c].body.kind, Some((Kind::History(_), _))));
        regular.count() > 1
    }

    /// Resolve every target written on node `i`.
    fn check_targets(&self, i: usize, errors: &mut Vec<syn::Error>) {
        let body = self.nodes[i].body;
        let mut lists: Vec<&[Transition]> = Vec::new();
        lists.extend(body.on.iter().flatten().map(|(_, ts)| ts.as_slice()));
        lists.extend(body.after.iter().flatten().map(|(_, ts)| ts.as_slice()));
        lists.extend(body.always.as_deref());
        lists.extend(body.on_done.as_ref().map(|(_, ts)| ts.as_slice()));
        for invoke in &body.invoke {
            lists.extend(invoke.on_done.as_deref());
            lists.extend(invoke.on_error.as_deref());
        }
        let targets = lists.into_iter().flatten().flat_map(|t| &t.targets);
        for target in targets.chain(body.target.as_ref().map(|(_, t)| t)) {
            if let Err(message) = self.resolve(i, target) {
                errors.push(target.name.error(message));
            }
        }
    }

    /// Mirror of the runtime resolver; returns the node index or a diagnostic.
    fn resolve(&self, source: usize, target: &Target) -> Result<usize, String> {
        let path = target.path();
        let found = match target.kind {
            TargetKind::Id => self.custom.get(path).or_else(|| self.by_id.get(path)).copied(),
            TargetKind::Child => self.descend(source, path).or_else(|| {
                let parent = self.nodes[source].parent.filter(|_| !self.strict_targets);
                parent.and_then(|p| self.descend(p, path))
            }),
            TargetKind::Plain => {
                let mut scope = Some(self.nodes[source].parent.unwrap_or(0));
                let mut hit = None;
                while let Some(s) = scope
                    && hit.is_none()
                {
                    hit = self.descend(s, path);
                    scope = self.nodes[s].parent;
                }
                hit.or_else(|| (path == self.nodes[0].id).then_some(0))
            }
        };
        let raw = &target.name.value;
        let from = &self.nodes[source].id;
        match found {
            Some(0) => Err(format!("`{raw}` targets the machine root, which is never legal")),
            Some(found) => Ok(found),
            None => Err(match target.kind {
                TargetKind::Id => {
                    format!("unknown target `{raw}`: no state has the custom or full id `{path}`")
                }
                TargetKind::Child => {
                    format!("unknown target `{raw}`: `{from}` has no descendant `{path}`")
                }
                TargetKind::Plain => format!(
                    "unknown target `{raw}` from `{from}`: not a sibling (or a sibling of an \
                     ancestor); use `.child` for children or `#id.path` for any state"
                ),
            }),
        }
    }

    fn descend(&self, from: usize, path: &str) -> Option<usize> {
        path.split('.').try_fold(from, |node, segment| self.child(node, segment))
    }

    fn child(&self, node: usize, key: &str) -> Option<usize> {
        self.nodes[node].children.iter().copied().find(|&c| self.nodes[c].name.value == key)
    }

    fn child_names(&self, node: usize) -> String {
        let names: Vec<&str> =
            self.nodes[node].children.iter().map(|&c| self.nodes[c].name.value.as_str()).collect();
        names.join(", ")
    }
}
