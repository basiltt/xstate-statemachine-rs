//! `xsm inspect`: everything about a machine on one screen.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Map, Value, json};
use xstate_core::machine::{NodeKind, Trigger};
use xstate_core::{Machine, StateId};

use super::analysis::{Analysis, analyse};
use super::live::{LiveOptions, run_inspect_live};
use super::model::{action_list, kind_name, policy_rows, short_id, transition_rows};
use crate::output::{self, exit_for, print_json};
use crate::ui::{Cell, Role, Table, Ui, print_lines};

/// Arguments of `inspect`.
#[derive(Debug, clap::Args)]
pub(crate) struct InspectArgs {
    /// Machine JSON file.
    file: PathBuf,
    /// Emit the facts as JSON.
    #[arg(long)]
    json: bool,
    /// Skip the transitions table.
    #[arg(long)]
    no_events: bool,
    /// Serve the live inspector (Stately Inspector protocol over SSE) and drive the machine
    /// from stdin: `EVENT`, `EVENT {"json": "payload"}`, `+500`, `reset`, `quit`.
    #[arg(long)]
    live: bool,
    #[command(flatten)]
    live_options: LiveOptions,
}

/// Run `inspect`.
pub(crate) fn run(ui: Ui, args: &InspectArgs) -> Result<u8> {
    if args.live {
        return run_inspect_live(ui, &args.file, &args.live_options);
    }
    let analysis = analyse(ui, &args.file, true, true);
    if args.json {
        print_json(&json_report(&analysis))?;
        return Ok(exit_for(analysis.ok()));
    }
    let Some(machine) = analysis.machine.as_ref() else {
        return Err(output::failure(analysis.first_error()));
    };
    println!();
    print_lines(&header(ui, &analysis, machine));
    println!();
    println!("{}", ui.rule("state tree"));
    print_lines(&tree(ui, machine));
    println!(
        "{}",
        ui.paint(Role::Muted, "  o atomic  + compound  || parallel  @ final  H history")
    );
    println!();
    if !args.no_events {
        print_transitions(ui, machine);
    }
    print_logic(ui, &analysis);
    print_policies(ui, machine);
    let warnings: Vec<_> = analysis.warnings().collect();
    if !warnings.is_empty() {
        println!("{}", ui.rule(&format!("warnings ({})", warnings.len())));
        for w in warnings {
            let at = if w.path.is_empty() {
                String::new()
            } else {
                format!("  {}", ui.paint(Role::Muted, &w.path))
            };
            println!("  {} {}{at}", ui.paint(Role::Warn, "!"), w.message);
        }
        println!();
    }
    Ok(output::EXIT_OK)
}

fn json_report(analysis: &Analysis) -> Value {
    let mut report = analysis.to_json();
    let Some(machine) = analysis.machine.as_ref() else { return report };
    let policies: Map<String, Value> =
        policy_rows(machine).into_iter().map(|(k, v)| (k.to_owned(), Value::String(v))).collect();
    let transitions: Vec<Value> = transition_rows(machine)
        .into_iter()
        .map(|t| {
            json!({ "event": t.event, "from": t.from, "to": t.to, "guard": t.guard, "actions": t.actions })
        })
        .collect();
    let kinds: Map<String, Value> = machine
        .states()
        .iter()
        .skip(1)
        .map(|n| (n.id.to_string(), Value::String(kind_name(&n.kind).to_owned())))
        .collect();
    if let Value::Object(map) = &mut report {
        map.insert("version".into(), machine.version().map_or(Value::Null, |v| json!(v)));
        map.insert("structure_hash".into(), json!(machine.structure_hash()));
        map.insert("policies".into(), Value::Object(policies));
        map.insert("transitions".into(), Value::Array(transitions));
        map.insert("state_kinds".into(), Value::Object(kinds));
    }
    report
}

fn header(ui: Ui, analysis: &Analysis, machine: &Machine) -> Vec<String> {
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for node in machine.states().iter().skip(1) {
        *kinds.entry(kind_name(&node.kind)).or_default() += 1;
    }
    let kind_text = kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ");
    let timers: usize = machine.states().iter().map(|n| n.after.len()).sum();
    let invokes: usize = machine.states().iter().map(|n| n.invokes.len()).sum();
    let logic = &analysis.logic;
    let mut rows: Vec<(&str, String)> = vec![("Machine", ui.paint(Role::Title, machine.id()))];
    if let Some(version) = machine.version() {
        rows.push(("Version", version.to_owned()));
    }
    rows.push(("Hash", machine.structure_hash().to_owned()));
    rows.push(("File", analysis.file.display().to_string()));
    rows.push(("States", format!("{}  ({kind_text})", machine.states().len() - 1)));
    rows.push((
        "Events",
        format!("{}    timers {timers}    invokes {invokes}", machine.declared_events().len()),
    ));
    rows.push((
        "Logic",
        format!(
            "{} actions, {} guards, {} services, {} named delays",
            logic.actions.len(),
            logic.guards.len(),
            logic.services.len(),
            logic.delays.len()
        ),
    ));
    ui.key_values(&rows)
}

fn tree(ui: Ui, machine: &Machine) -> Vec<String> {
    let always: HashSet<StateId> = machine
        .transitions()
        .iter()
        .filter(|t| t.trigger == Trigger::Always)
        .map(|t| t.source)
        .collect();
    let mut lines = vec![format!("  {}", node_line(ui, machine, StateId::ROOT, &always))];
    walk_children(ui, machine, StateId::ROOT, "  ", &always, &mut lines);
    lines
}

fn walk_children(
    ui: Ui,
    machine: &Machine,
    id: StateId,
    prefix: &str,
    always: &HashSet<StateId>,
    lines: &mut Vec<String>,
) {
    let children = &machine.node(id).children;
    let (branch, last, pipe) = if ui.unicode() {
        ("├── ", "└── ", "│   ")
    } else {
        ("|-- ", "`-- ", "|   ")
    };
    for (i, child) in children.iter().enumerate() {
        let is_last = i + 1 == children.len();
        let connector = if is_last { last } else { branch };
        lines.push(format!("{prefix}{connector}{}", node_line(ui, machine, *child, always)));
        let next = format!("{prefix}{}", if is_last { "    " } else { pipe });
        walk_children(ui, machine, *child, &next, always, lines);
    }
}

fn node_line(ui: Ui, machine: &Machine, id: StateId, always: &HashSet<StateId>) -> String {
    let node = machine.node(id);
    let (marker, role) = match &node.kind {
        NodeKind::Atomic => ("o", None),
        NodeKind::Compound { .. } => ("+", Some(Role::Key)),
        NodeKind::Parallel => ("||", Some(Role::Accent)),
        NodeKind::Final => ("@", Some(Role::Ok)),
        NodeKind::History { .. } => ("H", Some(Role::Muted)),
    };
    let marker = role.map_or_else(|| marker.to_owned(), |r| ui.paint(r, marker));
    let label = if node.parent.is_none() { node.id.to_string() } else { node.key.to_string() };
    let mut notes: Vec<String> = Vec::new();
    let is_initial = node.parent.is_some_and(
        |p| matches!(machine.node(p).kind, NodeKind::Compound { initial } if initial == id),
    );
    if is_initial {
        notes.push("(initial)".into());
    }
    if let NodeKind::History { deep, .. } = &node.kind {
        notes.push(if *deep { "(deep)" } else { "(shallow)" }.into());
    }
    if !node.entry.is_empty() {
        notes.push(format!("entry: {}", action_list(&node.entry)));
    }
    if !node.exit.is_empty() {
        notes.push(format!("exit: {}", action_list(&node.exit)));
    }
    if !node.after.is_empty() {
        let keys: Vec<&str> = node.after.iter().map(|a| a.key.as_str()).collect();
        notes.push(format!("after {}", keys.join(", ")));
    }
    if !node.invokes.is_empty() {
        let srcs: Vec<&str> = node.invokes.iter().map(|i| i.src.as_str()).collect();
        notes.push(format!("invoke {}", srcs.join(", ")));
    }
    if always.contains(&id) {
        notes.push("always".into());
    }
    if !node.tags.is_empty() {
        notes.push(node.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
    }
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!("  {}", ui.paint(Role::Muted, &notes.join("  ")))
    };
    format!("{marker} {label}{suffix}")
}

fn print_transitions(ui: Ui, machine: &Machine) {
    let rows = transition_rows(machine);
    if rows.is_empty() {
        return;
    }
    println!("{}", ui.rule(&format!("transitions ({})", rows.len())));
    let mut table = Table::new(&["Source", "Event", "Guard", "Actions", "Target"]);
    for row in &rows {
        let target = if row.to.starts_with('(') {
            Cell::styled(Role::Muted, row.to.clone())
        } else {
            Cell::text(
                row.to.split(", ").map(|t| short_id(machine, t)).collect::<Vec<_>>().join(", "),
            )
        };
        table.add(vec![
            Cell::text(short_id(machine, &row.from)),
            Cell::styled(Role::Accent, row.event.clone()),
            Cell::text(row.guard.clone()),
            Cell::text(row.actions.clone()),
            target,
        ]);
    }
    print_lines(&table.render(ui));
    println!();
}

fn print_logic(ui: Ui, analysis: &Analysis) {
    println!("{}", ui.rule("logic to implement"));
    let list = |names: &std::collections::BTreeSet<String>| {
        if names.is_empty() {
            ui.paint(Role::Muted, "none")
        } else {
            names.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    let logic = &analysis.logic;
    let rows = vec![
        ("actions", list(&logic.actions)),
        ("guards", list(&logic.guards)),
        ("services", list(&logic.services)),
        ("delays", list(&logic.delays)),
    ];
    print_lines(&ui.key_values(&rows).into_iter().map(|l| format!("  {l}")).collect::<Vec<_>>());
    println!();
}

fn print_policies(ui: Ui, machine: &Machine) {
    println!("{}", ui.rule("policies"));
    let rows = policy_rows(machine);
    print_lines(&ui.key_values(&rows).into_iter().map(|l| format!("  {l}")).collect::<Vec<_>>());
    println!();
}
