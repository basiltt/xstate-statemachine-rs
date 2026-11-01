//! `xsm paths`: a path to every reachable configuration, or every simple path.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::ValueEnum;
use serde_json::{Value, json};
use xstate_core::graph::{
    GraphOptions, GuardPolicy, Path, Weight, shortest_paths, simple_paths, stub_logic,
};
use xstate_core::{DynContext, Machine, SimulatedClock, SyncInterpreter};

use super::analysis::load_machine;
use super::model::short_id;
use crate::output::{self, print_json};
use crate::ui::{Cell, Role, Table, Ui, print_lines};

/// What stub guards return while exploring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum GuardsArg {
    /// Every guard is true.
    True,
    /// Every guard is false.
    False,
    /// Explore both and record which assumption each path relies on.
    Both,
}

impl GuardsArg {
    fn policy(self) -> GuardPolicy {
        match self {
            Self::True => GuardPolicy::True,
            Self::False => GuardPolicy::False,
            Self::Both => GuardPolicy::Both,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::True => "true",
            Self::False => "false",
            Self::Both => "both",
        }
    }
}

/// What the shortest-path search minimises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum WeightArg {
    /// Fewest steps.
    Steps,
    /// Least simulated time spent in `after` delays.
    Time,
}

/// Arguments of `paths`.
#[derive(Debug, clap::Args)]
pub(crate) struct PathsArgs {
    /// Machine JSON file.
    file: PathBuf,
    /// Every acyclic path instead of one shortest path per configuration.
    #[arg(long)]
    simple: bool,
    /// What stub guards return while exploring.
    #[arg(long, value_enum, default_value_t = GuardsArg::True)]
    guards: GuardsArg,
    /// Depth bound, in steps.
    #[arg(long, default_value_t = 50, value_name = "N")]
    max_depth: usize,
    /// Cap for `--simple`.
    #[arg(long, default_value_t = 1000, value_name = "N")]
    max_paths: usize,
    /// Shortest by step count or by simulated time.
    #[arg(long, value_enum, default_value_t = WeightArg::Steps)]
    weight: WeightArg,
    /// Emit the paths as JSON.
    #[arg(long)]
    json: bool,
}

/// Run `paths`.
pub(crate) fn run(ui: Ui, args: &PathsArgs) -> Result<u8> {
    let machine = load_machine(ui, &args.file)?;
    let policy = args.guards.policy();
    let logic = stub_logic::<DynContext>(&machine, policy);
    let options = GraphOptions {
        max_depth: args.max_depth,
        max_paths: args.max_paths,
        guards: policy,
        weight: match args.weight {
            WeightArg::Steps => Weight::Steps,
            WeightArg::Time => Weight::Time,
        },
        ..GraphOptions::default()
    };
    let explored = if args.simple {
        simple_paths(&machine, &logic, &options)
    } else {
        shortest_paths(&machine, &logic, &options)
    };
    let mut found = explored.map_err(|e| {
        output::failure(format!("{}: cannot be explored: {e}", args.file.display()))
    })?;
    if !args.simple && args.weight == WeightArg::Steps {
        found.sort_by(|a, b| {
            a.steps.len().cmp(&b.steps.len()).then_with(|| a.state_ids.cmp(&b.state_ids))
        });
    }
    ui.verbose(&format!("{} paths found", found.len()));
    if args.json {
        print_json(&json_report(&machine, args, &found)?)?;
    } else {
        print_report(ui, &machine, args, &found);
    }
    Ok(output::EXIT_OK)
}

/// Guard assumptions a path relies on, as `guards=true` / `guards=false`.
fn assumptions(path: &Path, policy: GuardPolicy) -> BTreeSet<String> {
    if policy != GuardPolicy::Both {
        return BTreeSet::new();
    }
    path.steps.iter().map(|s| format!("guards={}", s.guards)).collect()
}

fn mode_name(args: &PathsArgs) -> &'static str {
    if args.simple { "simple" } else { "shortest" }
}

fn print_report(ui: Ui, machine: &Machine, args: &PathsArgs, found: &[Path]) {
    println!();
    println!(
        "  {}  {} {} ({}, guards={})",
        ui.paint(Role::Key, machine.id()),
        found.len(),
        if found.len() == 1 { "path" } else { "paths" },
        mode_name(args),
        args.guards.name()
    );
    println!();
    let mut table = Table::new(&["Configuration", "Events", "Assumes"]);
    for path in found {
        let config: Vec<&str> = path.state_ids.iter().map(|s| short_id(machine, s)).collect();
        let events = path.event_string();
        let events = if events.is_empty() {
            Cell::styled(Role::Muted, "(initial)")
        } else {
            Cell::styled(Role::Accent, events)
        };
        let assumed = assumptions(path, args.guards.policy()).into_iter().collect::<Vec<_>>();
        table.add(vec![Cell::text(config.join(", ")), events, Cell::text(assumed.join("; "))]);
    }
    print_lines(&table.render(ui));
    println!();
}

/// Active leaf ids right after `start`, with every guard `value`.
fn initial_ids(machine: &Machine, value: bool) -> Result<Vec<String>> {
    let policy = if value { GuardPolicy::True } else { GuardPolicy::False };
    let logic = stub_logic::<DynContext>(machine, policy);
    let mut interp =
        SyncInterpreter::with_clock(machine.clone(), logic, Arc::new(SimulatedClock::new()));
    interp.start()?;
    Ok(interp.state_ids())
}

fn json_report(machine: &Machine, args: &PathsArgs, found: &[Path]) -> Result<Value> {
    let initial = [initial_ids(machine, true)?, initial_ids(machine, false)?];
    let policy = args.guards.policy();
    let paths: Vec<Value> = found
        .iter()
        .map(|path| {
            let mut from = initial[usize::from(!path.initial_guards)].clone();
            let mut steps = Vec::with_capacity(path.steps.len());
            for step in &path.steps {
                let assumed: Vec<String> = if policy == GuardPolicy::Both {
                    vec![format!("guards={}", step.guards)]
                } else {
                    Vec::new()
                };
                steps.push(json!({
                    "event": step.event.ty.as_str(),
                    "delay_ms": step.delay_ms,
                    "from": from,
                    "to": step.state_ids,
                    "assumptions": assumed,
                }));
                from.clone_from(&step.state_ids);
            }
            json!({
                "final_states": path.state_ids,
                "events": path.event_string(),
                "steps": steps,
            })
        })
        .collect();
    Ok(json!({
        "machine": machine.id(),
        "mode": mode_name(args),
        "guards": args.guards.name(),
        "weight": match args.weight { WeightArg::Steps => "steps", WeightArg::Time => "time" },
        "paths": paths,
    }))
}
