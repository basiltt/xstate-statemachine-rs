//! Loading a machine file and deriving facts about it: findings, logic names, reachability.
//!
//! Shared by `validate`, `inspect`, `docs`, `diagram`, `paths`, `simulate` and `snapshots`. The
//! CLI never re-implements validation: it builds the machine with the real engine and reports
//! what the engine says.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use xstate_core::config::ParseOptions;
use xstate_core::event::names;
use xstate_core::graph::{GraphOptions, GuardPolicy, reachable_states, stub_logic};
use xstate_core::machine::{ActionDef, Builtin, Delay, Guard, Trigger};
use xstate_core::{DynContext, Machine};

use crate::output;
use crate::ui::Ui;

/// A machine definition is a chart, not a data dump: refuse larger files before parsing.
const MAX_MACHINE_JSON_BYTES: u64 = 16 * 1024 * 1024;
/// Depth of the static reachability pass (mirrors the Python CLI's engine pass).
const REACHABILITY_DEPTH: usize = 8;
/// Configuration budget of the static reachability pass; wide parallel charts stop here.
const REACHABILITY_MAX_CONFIGS: usize = 20_000;

/// Severity of a [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// The file is unusable.
    Error,
    /// The file works but something looks wrong.
    Warning,
}

impl Level {
    /// Lower-case name used in `--json`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One validation finding.
#[derive(Debug, Clone)]
pub(crate) struct Finding {
    /// Severity.
    pub(crate) level: Level,
    /// Location inside the document (empty when unknown).
    pub(crate) path: String,
    /// What was found.
    pub(crate) message: String,
}

impl Finding {
    fn new(level: Level, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { level, path: path.into(), message: message.into() }
    }

    /// `{level, path, message}`.
    pub(crate) fn to_json(&self) -> Value {
        json!({ "level": self.level.as_str(), "path": self.path, "message": self.message })
    }
}

/// Every implementation name a machine refers to, by kind. Built-in actions are excluded.
#[derive(Debug, Default)]
pub(crate) struct LogicNames {
    pub(crate) actions: BTreeSet<String>,
    pub(crate) guards: BTreeSet<String>,
    pub(crate) services: BTreeSet<String>,
    pub(crate) delays: BTreeSet<String>,
}

impl LogicNames {
    /// Walk the compiled machine and collect the names.
    pub(crate) fn collect(machine: &Machine) -> Self {
        let mut names = Self::default();
        for node in machine.states() {
            names.action_list(&node.entry);
            names.action_list(&node.exit);
            names.services.extend(node.invokes.iter().map(|i| i.src.clone()));
            for after in &node.after {
                if let Delay::Named(name) = &after.delay {
                    names.delays.insert(name.clone());
                }
            }
        }
        for transition in machine.transitions() {
            names.action_list(&transition.actions);
            if let Some(guard) = &transition.guard {
                names.guard(guard);
            }
        }
        names
    }

    fn action_list(&mut self, actions: &[ActionDef]) {
        for action in actions {
            match action {
                ActionDef::User { name, .. } => {
                    self.actions.insert(name.to_string());
                }
                ActionDef::Builtin { builtin, .. } => self.builtin(builtin),
            }
        }
    }

    fn builtin(&mut self, builtin: &Builtin) {
        match builtin {
            Builtin::Raise { delay, .. } | Builtin::SendTo { delay, .. } => {
                if let Some(Delay::Named(name)) = delay {
                    self.delays.insert(name.clone());
                }
            }
            Builtin::SpawnChild { src, .. } => {
                self.services.insert(src.clone());
            }
            Builtin::Choose(branches) => {
                for branch in branches {
                    if let Some(guard) = &branch.guard {
                        self.guard(guard);
                    }
                    self.action_list(&branch.actions);
                }
            }
            _ => {}
        }
    }

    fn guard(&mut self, guard: &Guard) {
        match guard {
            Guard::Named { name, .. } => {
                self.guards.insert(name.clone());
            }
            Guard::StateIn { .. } => {}
            Guard::Not(inner) => self.guard(inner),
            Guard::And(list) | Guard::Or(list) => list.iter().for_each(|g| self.guard(g)),
        }
    }
}

/// Everything the CLI knows about one machine file.
#[derive(Debug)]
pub(crate) struct Analysis {
    /// The file as given on the command line.
    pub(crate) file: PathBuf,
    /// Machine id (from the compiled machine, else the document, else the file stem).
    pub(crate) id: String,
    /// The compiled machine; `None` when the file could not be built.
    pub(crate) machine: Option<Machine>,
    /// Errors first-found order, then warnings.
    pub(crate) findings: Vec<Finding>,
    /// Names the machine needs implementations for.
    pub(crate) logic: LogicNames,
    /// State ids never active within the reachability bound.
    pub(crate) unreachable: Vec<String>,
}

impl Analysis {
    /// `true` when there is no error-level finding.
    pub(crate) fn ok(&self) -> bool {
        !self.findings.iter().any(|f| f.level == Level::Error)
    }

    /// Error-level findings.
    pub(crate) fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.level == Level::Error)
    }

    /// Warning-level findings.
    pub(crate) fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.level == Level::Warning)
    }

    /// Number of states below the root, and how many of them are top-level.
    pub(crate) fn state_counts(&self) -> (usize, usize) {
        self.machine.as_ref().map_or((0, 0), |m| {
            (m.states().len().saturating_sub(1), m.states().first().map_or(0, |r| r.children.len()))
        })
    }

    /// The first error message, prefixed with the file name.
    pub(crate) fn first_error(&self) -> String {
        let detail = self.errors().next().map_or_else(
            || "unknown error".to_owned(),
            |f| {
                if f.path.is_empty() {
                    f.message.clone()
                } else {
                    format!("{} ({})", f.message, f.path)
                }
            },
        );
        format!("{}: {detail}", self.file.display())
    }

    /// The machine, or a failure error (exit 1) naming the first problem.
    pub(crate) fn into_machine(self) -> Result<Machine> {
        match self.machine {
            Some(machine) => Ok(machine),
            None => Err(output::failure(self.first_error())),
        }
    }

    /// The shape shared by `validate --json` and `inspect --json`.
    pub(crate) fn to_json(&self) -> Value {
        let (nested, top) = self.state_counts();
        let state_ids: Vec<&str> = self
            .machine
            .as_ref()
            .map(|m| m.states().iter().skip(1).map(|n| &*n.id).collect())
            .unwrap_or_default();
        let events: Vec<&String> =
            self.machine.as_ref().map(|m| m.declared_events().iter().collect()).unwrap_or_default();
        json!({
            "file": self.file.display().to_string(),
            "ok": self.ok(),
            "machine": self.id,
            "states": nested,
            "top_level_states": top,
            "state_ids": state_ids,
            "events": events,
            "actions": self.logic.actions,
            "guards": self.logic.guards,
            "services": self.logic.services,
            "delays": self.logic.delays,
            "unreachable_states": self.unreachable,
            "findings": self.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Read and parse a JSON document, turning every failure into an error finding.
fn read_document(path: &Path) -> Result<Value, Finding> {
    let error = |path: &str, message: String| Finding::new(Level::Error, path, message);
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            error("", "file not found".into())
        } else {
            error("", format!("cannot read file: {e}"))
        }
    })?;
    if !meta.is_file() {
        return Err(error("", "not a file".into()));
    }
    if meta.len() > MAX_MACHINE_JSON_BYTES {
        return Err(error(
            "",
            format!("file is {} bytes; the limit is {MAX_MACHINE_JSON_BYTES}", meta.len()),
        ));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| error("", format!("cannot read file: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| error(&format!("line {}", e.line()), format!("invalid JSON: {e}")))
}

fn engine_error_finding(err: &xstate_core::Error) -> Finding {
    match err {
        xstate_core::Error::InvalidConfig { path, message } => {
            Finding::new(Level::Error, path.clone(), message.clone())
        }
        other => Finding::new(Level::Error, "", other.to_string()),
    }
}

/// `true` for a final state whose parent completes through `onDone`: entering it raises the
/// parent's done event inside the same macrostep, so it is never part of a stable configuration
/// and the explorer would report it although it is entered.
fn is_transient_final(machine: &Machine, id: &str, unreachable: &[String]) -> bool {
    let Some(node) = machine.state_by_id(id).map(|s| machine.node(s)) else { return false };
    let Some(parent) = node.parent else { return false };
    let parent_id = &machine.node(parent).id;
    let done = names::done_state(parent_id);
    node.is_final()
        && !unreachable.iter().any(|u| u == &**parent_id)
        && machine
            .transitions()
            .iter()
            .any(|t| t.source == parent && matches!(&t.trigger, Trigger::Event(e) if *e == done))
}

/// State ids the engine never enters within the reachability bound, guards assumed either way.
fn unreachable_states(machine: &Machine) -> Result<Vec<String>, xstate_core::Error> {
    let logic = stub_logic::<DynContext>(machine, GuardPolicy::Both);
    let options = GraphOptions {
        max_depth: REACHABILITY_DEPTH,
        max_configs: REACHABILITY_MAX_CONFIGS,
        guards: GuardPolicy::Both,
        ..GraphOptions::default()
    };
    let found = reachable_states(machine, &logic, &options)?.unreachable;
    let kept =
        found.iter().filter(|id| !is_transient_final(machine, id, &found)).cloned().collect();
    Ok(kept)
}

/// Load, build and inspect one machine file. Never fails: problems become findings.
///
/// `lenient` turns unknown configuration keys into warnings instead of errors;
/// `reachability` adds the static unreachable-state pass.
pub(crate) fn analyse(ui: Ui, path: &Path, lenient: bool, reachability: bool) -> Analysis {
    let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let mut analysis = Analysis {
        file: path.to_path_buf(),
        id: stem,
        machine: None,
        findings: Vec::new(),
        logic: LogicNames::default(),
        unreachable: Vec::new(),
    };
    let document = match read_document(path) {
        Ok(document) => document,
        Err(finding) => {
            analysis.findings.push(finding);
            return analysis;
        }
    };
    if let Some(id) = document.get("id").and_then(Value::as_str) {
        id.clone_into(&mut analysis.id);
    }
    let options = ParseOptions { strict_config: Some(!lenient) };
    let machine = match Machine::from_value_with(&document, &options) {
        Ok(machine) => machine,
        Err(err) => {
            analysis.findings.push(engine_error_finding(&err));
            return analysis;
        }
    };
    ui.verbose(&format!(
        "{}: compiled '{}' ({} states, {} transitions)",
        path.display(),
        machine.id(),
        machine.states().len(),
        machine.transitions().len()
    ));
    machine.id().clone_into(&mut analysis.id);
    analysis.logic = LogicNames::collect(&machine);
    for w in machine.warnings() {
        analysis.findings.push(Finding::new(Level::Warning, w.path.clone(), w.message.clone()));
    }
    if reachability {
        match unreachable_states(&machine) {
            Ok(ids) => {
                for id in &ids {
                    analysis.findings.push(Finding::new(
                        Level::Warning,
                        id.clone(),
                        format!(
                            "state is not reachable within {REACHABILITY_DEPTH} steps \
                             (guards assumed either way)"
                        ),
                    ));
                }
                analysis.unreachable = ids;
            }
            Err(err) => analysis.findings.push(Finding::new(
                Level::Warning,
                "",
                format!("reachability analysis skipped: {err}"),
            )),
        }
    }
    analysis.machine = Some(machine);
    analysis
}

/// Load a machine for a command that only needs the compiled model (lenient parsing).
pub(crate) fn load_machine(ui: Ui, path: &Path) -> Result<Machine> {
    analyse(ui, path, true, false).into_machine()
}
