//! `xsm validate`: build each file with the real engine and report what it finds.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::analysis::{Analysis, analyse};
use super::model::short_id;
use crate::output::{exit_for, print_json};
use crate::ui::{Cell, Role, Table, Ui, print_lines};

/// Arguments of `validate`.
#[derive(Debug, clap::Args)]
pub(crate) struct ValidateArgs {
    /// Machine JSON files to check.
    #[arg(required = true, value_name = "FILES")]
    files: Vec<PathBuf>,
    /// Emit one JSON array with a record per file.
    #[arg(long)]
    json: bool,
    /// Report unknown configuration keys as warnings instead of errors.
    #[arg(long)]
    lenient: bool,
}

/// Run `validate`; exit 1 when any file has an error.
pub(crate) fn run(ui: Ui, args: &ValidateArgs) -> Result<u8> {
    let results: Vec<Analysis> =
        args.files.iter().map(|f| analyse(ui, f, args.lenient, true)).collect();
    let all_ok = results.iter().all(Analysis::ok);
    if args.json {
        let records: Vec<Value> = results.iter().map(Analysis::to_json).collect();
        print_json(&records)?;
        return Ok(exit_for(all_ok));
    }
    println!();
    for analysis in &results {
        print_lines(&file_report(ui, analysis));
    }
    println!();
    if results.len() > 1 {
        print_lines(&summary_table(ui, &results));
        println!();
    }
    let bad = results.iter().filter(|a| !a.ok()).count();
    if bad > 0 {
        eprintln!("{bad} file(s) had errors.");
    } else {
        let warnings: usize = results.iter().map(|a| a.warnings().count()).sum();
        let tail = if warnings > 0 { format!(" ({warnings} warning(s))") } else { String::new() };
        println!("All {} file(s) are valid.{tail}", results.len());
    }
    Ok(exit_for(all_ok))
}

fn file_report(ui: Ui, analysis: &Analysis) -> Vec<String> {
    let name = analysis.file.display().to_string();
    let mut lines = Vec::new();
    if !analysis.ok() {
        let count = analysis.errors().count();
        lines.push(format!("  {} {name} -- {count} issue(s):", ui.paint(Role::Err, "FAIL")));
        for finding in analysis.errors() {
            let at = if finding.path.is_empty() {
                String::new()
            } else {
                format!("  {}", ui.paint(Role::Muted, &format!("@ {}", finding.path)))
            };
            lines.push(format!("      - {}{at}", finding.message));
        }
        return lines;
    }
    lines.push(format!("  {}   {name}", ui.paint(Role::Ok, "ok")));
    lines.extend(detail_rows(ui, analysis).into_iter().map(|l| format!("      {l}")));
    for warning in analysis.warnings() {
        let at = if warning.path.is_empty() {
            String::new()
        } else {
            format!("  {}", ui.paint(Role::Muted, &warning.path))
        };
        lines.push(format!("      {} {}{at}", ui.paint(Role::Warn, "!"), warning.message));
    }
    lines
}

/// `N  (a, b, c)`, or just `0` when empty.
fn counted<I: IntoIterator<Item = String>>(names: I) -> String {
    let names: Vec<String> = names.into_iter().collect();
    if names.is_empty() {
        "0".to_owned()
    } else {
        format!("{}  ({})", names.len(), names.join(", "))
    }
}

fn detail_rows(ui: Ui, analysis: &Analysis) -> Vec<String> {
    let Some(machine) = analysis.machine.as_ref() else { return Vec::new() };
    let states = machine.states().iter().skip(1).map(|n| short_id(machine, &n.id).to_owned());
    let logic = &analysis.logic;
    let rows: Vec<(&str, String)> = vec![
        ("Machine:", analysis.id.clone()),
        ("States:", counted(states)),
        ("Events:", counted(machine.declared_events().iter().cloned())),
        ("Actions:", counted(logic.actions.iter().cloned())),
        ("Guards:", counted(logic.guards.iter().cloned())),
        ("Services:", counted(logic.services.iter().cloned())),
        ("Delays:", counted(logic.delays.iter().cloned())),
    ];
    ui.key_values(&rows)
}

fn summary_table(ui: Ui, results: &[Analysis]) -> Vec<String> {
    let mut table =
        Table::new(&["File", "Result", "States", "Events", "Warnings"]).right(2).right(3).right(4);
    for analysis in results {
        let result = if analysis.ok() {
            Cell::styled(Role::Ok, "ok")
        } else {
            Cell::styled(Role::Err, "FAIL")
        };
        let events = analysis.machine.as_ref().map_or(0, |m| m.declared_events().len());
        table.add(vec![
            Cell::text(analysis.file.display().to_string()),
            result,
            Cell::text(analysis.state_counts().0.to_string()),
            Cell::text(events.to_string()),
            Cell::text(analysis.warnings().count().to_string()),
        ]);
    }
    table.render(ui)
}
