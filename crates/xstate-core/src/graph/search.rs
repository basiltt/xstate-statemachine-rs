//! Search strategies over the explorer: breadth-first, Dijkstra and depth-first, all
//! recording their discoveries in an [`Arena`] of parent-linked nodes.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, VecDeque};

use super::explore::{ConfigKey, Edge, Explorer, Root};
use super::{Path, PathStep};
use crate::context::Context;
use crate::engine::{ActiveState, Status};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::machine::Machine;

/// How a node was reached.
pub(crate) struct Link {
    parent: usize,
    event: Event,
    delay_ms: Option<f64>,
}

/// A discovered configuration.
pub(crate) struct Node<C> {
    pub(crate) state: ActiveState<C>,
    key: ConfigKey,
    pub(crate) depth: usize,
    cost: f64,
    guards: bool,
    link: Option<Link>,
}

impl<C> Node<C> {
    fn root(root: Root<C>) -> Self {
        Self {
            state: root.state,
            key: root.key,
            depth: 0,
            cost: 0.0,
            guards: root.guards,
            link: None,
        }
    }

    fn child(parent: usize, depth: usize, cost: f64, edge: Edge<C>) -> Self {
        Self {
            state: edge.state,
            key: edge.key,
            depth,
            cost,
            guards: edge.guards,
            link: Some(Link { parent, event: edge.event, delay_ms: edge.delay_ms }),
        }
    }

    fn is_running(&self) -> bool {
        self.state.status() == Status::Running
    }
}

/// Discovered nodes in discovery order, indexed by configuration for the searches that
/// deduplicate.
pub(crate) struct Arena<C> {
    nodes: Vec<Node<C>>,
    index: HashMap<ConfigKey, usize>,
}

impl<C: Context> Arena<C> {
    fn new() -> Self {
        Self { nodes: Vec::new(), index: HashMap::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn nodes(&self) -> &[Node<C>] {
        &self.nodes
    }

    fn contains(&self, key: &ConfigKey) -> bool {
        self.index.contains_key(key)
    }

    /// Append `node`, indexing its configuration.
    fn insert(&mut self, node: Node<C>, max_configs: usize) -> Result<usize> {
        self.index.insert(node.key.clone(), self.nodes.len());
        let idx = self.push(node);
        if self.nodes.len() > max_configs {
            return Err(Error::LimitExceeded(format!(
                "graph: more than {max_configs} reachable configurations; raise `max_configs`, \
                 lower `max_depth`, or explore a sub-chart"
            )));
        }
        Ok(idx)
    }

    fn push(&mut self, node: Node<C>) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// `true` when `key` is the configuration of `idx` or of one of its ancestors.
    fn on_path(&self, mut idx: usize, key: &ConfigKey) -> bool {
        loop {
            let node = &self.nodes[idx];
            if node.key == *key {
                return true;
            }
            match &node.link {
                Some(link) => idx = link.parent,
                None => return false,
            }
        }
    }

    /// The path from the root to node `idx`.
    pub(crate) fn path(&self, machine: &Machine, idx: usize) -> Result<Path> {
        let mut chain = vec![idx];
        while let Some(link) = &self.nodes[chain[chain.len() - 1]].link {
            chain.push(link.parent);
        }
        chain.reverse();
        let mut steps = Vec::with_capacity(chain.len() - 1);
        for &i in &chain[1..] {
            let node = &self.nodes[i];
            let Some(link) = &node.link else { continue };
            steps.push(PathStep {
                event: link.event.clone(),
                state_ids: node.state.leaf_ids(machine),
                context: serde_json::to_value(node.state.context())?,
                guards: node.guards,
                delay_ms: link.delay_ms,
            });
        }
        let last = &self.nodes[idx];
        Ok(Path {
            steps,
            state_ids: last.state.leaf_ids(machine),
            context: serde_json::to_value(last.state.context())?,
            initial_guards: self.nodes[chain[0]].guards,
        })
    }
}

/// Breadth-first search: the first path found into each configuration is a shortest one.
pub(crate) fn bfs<C: Context>(explorer: &mut Explorer<'_, C>) -> Result<Arena<C>> {
    let options = explorer.options();
    let mut arena = Arena::new();
    let mut queue = VecDeque::new();
    for root in explorer.roots()? {
        if !arena.contains(&root.key) {
            queue.push_back(arena.insert(Node::root(root), options.max_configs)?);
        }
    }
    while let Some(idx) = queue.pop_front() {
        let node = &arena.nodes[idx];
        if node.depth >= options.max_depth || !node.is_running() {
            continue;
        }
        let depth = node.depth + 1;
        for edge in explorer.successors(&node.state)? {
            if !arena.contains(&edge.key) {
                let next = Node::child(idx, depth, 0.0, edge);
                queue.push_back(arena.insert(next, options.max_configs)?);
            }
        }
    }
    Ok(arena)
}

/// A non-negative path cost ordered with [`f64::total_cmp`].
#[derive(Clone, Copy, PartialEq)]
struct Cost(f64);

impl Eq for Cost {}

impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cost {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Candidate nodes ordered by (cost, depth, offer order); the nodes themselves wait in a
/// side table so the heap stays `Ord`.
struct Frontier<C> {
    heap: BinaryHeap<Reverse<(Cost, usize, usize)>>,
    pending: Vec<Option<Node<C>>>,
}

impl<C> Frontier<C> {
    fn new() -> Self {
        Self { heap: BinaryHeap::new(), pending: Vec::new() }
    }

    fn offer(&mut self, node: Node<C>) {
        self.heap.push(Reverse((Cost(node.cost), node.depth, self.pending.len())));
        self.pending.push(Some(node));
    }

    fn pop(&mut self) -> Option<Node<C>> {
        loop {
            let Reverse((_, _, seq)) = self.heap.pop()?;
            if let Some(node) = self.pending[seq].take() {
                return Some(node);
            }
        }
    }
}

/// Dijkstra over simulated time: timer steps cost their delay, events cost nothing; ties go
/// to the shorter path, then to discovery order.
pub(crate) fn dijkstra<C: Context>(explorer: &mut Explorer<'_, C>) -> Result<Arena<C>> {
    let options = explorer.options();
    let mut arena = Arena::new();
    let mut frontier = Frontier::new();
    for root in explorer.roots()? {
        frontier.offer(Node::root(root));
    }
    while let Some(node) = frontier.pop() {
        if arena.contains(&node.key) {
            continue;
        }
        let (depth, cost, running) = (node.depth, node.cost, node.is_running());
        let idx = arena.insert(node, options.max_configs)?;
        if depth >= options.max_depth || !running {
            continue;
        }
        for edge in explorer.successors(&arena.nodes[idx].state)? {
            if !arena.contains(&edge.key) {
                let weight = edge.delay_ms.unwrap_or(0.0);
                frontier.offer(Node::child(idx, depth + 1, cost + weight, edge));
            }
        }
    }
    Ok(arena)
}

/// Depth-first enumeration of acyclic paths. Returns the arena and the indices of the paths
/// found, in discovery order.
pub(crate) fn dfs<C: Context>(explorer: &mut Explorer<'_, C>) -> Result<(Arena<C>, Vec<usize>)> {
    let options = explorer.options();
    let mut arena = Arena::new();
    let mut stack = Vec::new();
    for root in explorer.roots()? {
        let idx = arena.push(Node::root(root));
        if arena.nodes[idx].is_running() {
            stack.push(idx);
        }
    }
    stack.reverse();
    let mut found = Vec::new();
    while found.len() < options.max_paths {
        let Some(idx) = stack.pop() else { break };
        let node = &arena.nodes[idx];
        if node.depth >= options.max_depth {
            continue;
        }
        let depth = node.depth + 1;
        let mut extend = Vec::new();
        for edge in explorer.successors(&node.state)? {
            if arena.on_path(idx, &edge.key) {
                continue;
            }
            let next = arena.push(Node::child(idx, depth, 0.0, edge));
            found.push(next);
            if found.len() >= options.max_paths {
                break;
            }
            if arena.nodes[next].is_running() {
                extend.push(next);
            }
        }
        stack.extend(extend.into_iter().rev());
    }
    Ok((arena, found))
}
