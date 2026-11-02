//! `xsm simulate`: a non-interactive run on a simulated clock with stub logic.
//!
//! Events come from `-e A,B,+500` (`+N` advances the clock N ms) or from a `--script` file;
//! guards are constants (`--guards-false`, or `{"guard": ...}` script commands). Context is
//! hidden unless `--context KEYS` asks for it: it is user data and may be sensitive.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value, json};
use xstate_core::{DynContext, Observer, Status};
use xstate_inspect::{InspectorObserver, JsonLinesSink};

use super::analysis::{LogicNames, load_machine};
use super::model::short_id;
use super::session::{
    Command, Session, Step, StepKind, load_script, parse_events, unknown_guard_message,
};
use crate::output::{self, exit_for, print_json};
use crate::ui::{Cell, Role, Table, Ui, print_lines};

/// `--guards-false` without names (every guard false) and `--context *` (every key).
const WILDCARD: &str = "*";

/// Arguments of `simulate`.
#[derive(Debug, clap::Args)]
pub(crate) struct SimulateArgs {
    /// Machine JSON file.
    file: PathBuf,
    /// Comma-separated events to send in order; `+500` advances the clock 500 ms.
    #[arg(short, long, value_name = "EVENTS")]
    events: Option<String>,
    /// JSON file: a list of `{"send"}`, `{"clock"}`, `{"guard","value"}` and `{"reset"}`
    /// commands.
    #[arg(long, value_name = "FILE")]
    script: Option<PathBuf>,
    /// Make guards return false: every guard when given bare, only the listed ones with
    /// `--guards-false=a,b`.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = WILDCARD,
        value_name = "NAMES"
    )]
    guards_false: Option<String>,
    /// Emit the final state and the history as JSON.
    #[arg(long)]
    json: bool,
    /// Show these context keys (comma-separated, or `*` for all); hidden by default.
    #[arg(long, value_name = "KEYS")]
    context: Option<String>,
    /// Append the session to PATH as Stately Inspector messages (JSON Lines), replayable with
    /// `xsm replay` and the Python CLI. Context and payloads are not recorded.
    #[arg(long, value_name = "PATH")]
    record: Option<PathBuf>,
}

/// Which part of the context the user asked to see.
#[derive(Debug)]
enum ContextSelection {
    Hidden,
    All,
    Keys(Vec<String>),
}

impl ContextSelection {
    fn parse(text: Option<&str>) -> Self {
        let Some(text) = text else { return Self::Hidden };
        let keys: Vec<String> =
            text.split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect();
        if keys.iter().any(|k| k == WILDCARD) {
            Self::All
        } else if keys.is_empty() {
            Self::Hidden
        } else {
            Self::Keys(keys)
        }
    }

    /// The visible part of `context`, or `None` when it stays hidden.
    fn select(&self, context: &Value) -> Option<Value> {
        match self {
            Self::Hidden => None,
            Self::All => Some(context.clone()),
            Self::Keys(keys) => {
                let mut picked = Map::new();
                if let Some(object) = context.as_object() {
                    for key in keys {
                        if let Some(value) = object.get(key) {
                            picked.insert(key.clone(), value.clone());
                        }
                    }
                }
                Some(Value::Object(picked))
            }
        }
    }
}

/// Split `--guards-false` into the default guard value and the explicit overrides.
fn parse_guards_false(
    arg: Option<&str>,
    known: &[String],
) -> Result<(bool, std::collections::BTreeMap<String, bool>)> {
    let mut overrides = std::collections::BTreeMap::new();
    let Some(arg) = arg else { return Ok((true, overrides)) };
    if arg == WILDCARD {
        return Ok((false, overrides));
    }
    for name in arg.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !known.iter().any(|k| k == name) {
            return Err(output::usage(unknown_guard_message(name, known)));
        }
        overrides.insert(name.to_owned(), false);
    }
    Ok((true, overrides))
}

/// The commands to run, from `-e` or `--script`.
fn build_commands(args: &SimulateArgs) -> Result<Vec<Command>> {
    if args.script.is_some() && args.events.is_some() {
        return Err(output::usage(
            "--script cannot be combined with --events; put every step in the script",
        ));
    }
    if let Some(script) = &args.script {
        return load_script(script).map_err(output::usage);
    }
    parse_events(args.events.as_deref().unwrap_or("")).map_err(output::usage)
}

/// Run `simulate`; exit 1 when a step reported an error.
pub(crate) fn run(ui: Ui, args: &SimulateArgs) -> Result<u8> {
    let machine = load_machine(ui, &args.file)?;
    let known: Vec<String> = LogicNames::collect(&machine).guards.into_iter().collect();
    let (default_guard, overrides) = parse_guards_false(args.guards_false.as_deref(), &known)?;
    let commands = build_commands(args)?;
    let recorder = args.record.as_deref().map(open_recorder).transpose()?;
    let mut session = Session::new(machine, default_guard, overrides, known, recorder)?;
    for command in &commands {
        session.apply(command)?;
    }
    let context = ContextSelection::parse(args.context.as_deref());
    if args.json {
        print_json(&json_report(&session, &context))?;
    } else {
        print_text(ui, &session, &context);
        if commands.is_empty() {
            // TODO(tui): an interactive terminal mode (pick events, advance the clock, undo).
            println!(
                "{}",
                ui.paint(
                    Role::Muted,
                    "no events given; use -e A,B,+500 or --script FILE (interactive mode is not \
                     implemented yet)"
                )
            );
            println!();
        }
    }
    Ok(exit_for(!session.had_errors()))
}

/// An inspector observer appending to the `--record` file.
fn open_recorder(path: &Path) -> Result<Arc<dyn Observer<DynContext>>> {
    let sink = JsonLinesSink::open(path).map_err(|e| {
        output::usage(format!("cannot open --record file '{}': {e}", path.display()))
    })?;
    Ok(Arc::new(InspectorObserver::new(sink)))
}

/// Names of the states with armed `after` timers in the active configuration.
fn armed_timers(session: &Session) -> Vec<String> {
    let machine = session.machine();
    let Some(state) = session.interpreter().state() else { return Vec::new() };
    let mut timers = Vec::new();
    for id in state.configuration() {
        let node = machine.node(id);
        for after in &node.after {
            timers.push(format!("{}:{}", short_id(machine, &node.id), after.key));
        }
    }
    timers
}

fn json_report(session: &Session, context: &ContextSelection) -> Value {
    let machine = session.machine();
    let interp = session.interpreter();
    let history = session.history();
    let initial = history.first();
    let overrides: Map<String, Value> =
        session.guard_overrides().iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let mut report = json!({
        "machine": machine.id(),
        "status": session.status().as_str(),
        "active": interp.state_ids(),
        "value": interp.state().map_or(Value::Null, |s| s.value(machine)),
        "clock_ms": session.clock_ms(),
        "pending_timers": interp.pending_timers(),
        "armed_timers": armed_timers(session),
        "default_guard": session.default_guard(),
        "guards": overrides,
        "initial": {
            "states": initial.map(|s| s.after.clone()).unwrap_or_default(),
            "actions": initial.map(|s| s.actions.clone()).unwrap_or_default(),
        },
        "history": history.iter().skip(1).map(Step::to_json).collect::<Vec<_>>(),
    });
    if let (Some(selected), Value::Object(map)) =
        (interp.context().and_then(|c| context.select(c)), &mut report)
    {
        map.insert("context".into(), selected);
    }
    report
}

fn states_text(session: &Session, ids: &[String]) -> String {
    let machine = session.machine();
    ids.iter().map(|id| short_id(machine, id)).collect::<Vec<_>>().join(", ")
}

fn result_cell(session: &Session, step: &Step) -> Cell {
    if let Some(error) = &step.error {
        return Cell::styled(Role::Err, format!("error: {error}"));
    }
    let always_show = matches!(step.kind, StepKind::Start | StepKind::Reset);
    if step.changed() || always_show {
        Cell::styled(Role::Ok, states_text(session, &step.after))
    } else if step.unhandled {
        Cell::styled(Role::Warn, "(unhandled)")
    } else {
        Cell::styled(Role::Muted, "(no change)")
    }
}

fn history_table(ui: Ui, session: &Session) -> Vec<String> {
    let mut table = Table::new(&["#", "Step", "States", "Actions", "Clock ms"]).right(0).right(4);
    for (index, step) in session.history().iter().enumerate() {
        let role = if step.kind == StepKind::Event { Role::Accent } else { Role::Key };
        table.add(vec![
            Cell::text(index.to_string()),
            Cell::styled(role, step.label.clone()),
            result_cell(session, step),
            Cell::styled(Role::Muted, step.actions.join(", ")),
            Cell::text(step.clock_ms.to_string()),
        ]);
    }
    table.render(ui)
}

fn status_role(status: Status) -> Role {
    match status {
        Status::Running => Role::Ok,
        Status::Done => Role::Title,
        Status::Error => Role::Err,
        Status::Stopped => Role::Muted,
    }
}

fn final_panel(ui: Ui, session: &Session, context: &ContextSelection) -> Vec<String> {
    let interp = session.interpreter();
    let timers = armed_timers(session);
    let mut guard_parts =
        vec![if session.default_guard() { "all true" } else { "all false" }.to_owned()];
    guard_parts
        .extend(session.guard_overrides().iter().map(|(name, value)| format!("{name}={value}")));
    let guards = guard_parts.join(", ");
    let mut rows: Vec<(&str, String)> = vec![
        ("status", ui.paint(status_role(session.status()), session.status().as_str())),
        ("states", states_text(session, &interp.state_ids())),
        ("clock", format!("{} ms", session.clock_ms())),
        ("timers", if timers.is_empty() { "none".to_owned() } else { timers.join(", ") }),
        ("guards", guards),
    ];
    if let Some(selected) = interp.context().and_then(|c| context.select(c)) {
        rows.push(("context", selected.to_string()));
    }
    ui.key_values(&rows).into_iter().map(|l| format!("  {l}")).collect()
}

fn print_text(ui: Ui, session: &Session, context: &ContextSelection) {
    println!();
    println!("  simulate {}", ui.paint(Role::Title, session.machine().id()));
    println!();
    print_lines(&history_table(ui, session));
    println!();
    println!("{}", ui.rule("final state"));
    print_lines(&final_panel(ui, session, context));
    println!();
}
