//! `xsm` — the command-line companion of xstate-statemachine.
//!
//! Validate, inspect, diagram, document, simulate and analyse XState machines, and inspect
//! snapshot stores, from the terminal.
//!
//! Exit codes: `0` success, `1` findings or a failed run (validation errors, stale snapshots,
//! a machine that does not build), `2` usage error or refused input (bad flag value, missing
//! store, malformed script).
//!
//! Presentation: colour is used only when stdout is a terminal and nothing disabled it
//! (`--plain`, `--no-color`, `NO_COLOR`, `XSM_NO_COLOR`); piped output is plain text.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)] // a CLI prints to stdout/stderr by design

mod commands;
mod output;
mod ui;

use std::process::ExitCode;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};

use commands::{
    diagram::DiagramArgs, docs::DocsArgs, info::InfoArgs, inspect::InspectArgs, paths::PathsArgs,
    simulate::SimulateArgs, snapshots::SnapshotsArgs, validate::ValidateArgs,
};
use ui::Ui;

/// Statecharts for Rust: validate, inspect, simulate and operate XState machines.
#[derive(Debug, Parser)]
#[command(
    name = "xsm",
    version,
    about,
    long_about = None,
    disable_version_flag = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Plain text: no colour, no box-drawing glyphs.
    #[arg(long, global = true)]
    plain: bool,
    /// Keep the layout but emit no colour (also honours `NO_COLOR` and `XSM_NO_COLOR`).
    #[arg(long, global = true)]
    no_color: bool,
    /// Print diagnostics on stderr while a command runs.
    #[arg(long, global = true)]
    verbose: bool,
    /// Print version information and exit.
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    version: Option<bool>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build each file with the real engine and report every finding.
    #[command(visible_alias = "val")]
    Validate(ValidateArgs),
    /// Show a machine's state tree, transitions, logic and policies.
    #[command(visible_alias = "ins")]
    Inspect(InspectArgs),
    /// Export a Mermaid, `PlantUML` or JSON diagram.
    #[command(visible_alias = "dia")]
    Diagram(DiagramArgs),
    /// Generate a Markdown reference page per machine.
    Docs(DocsArgs),
    /// Run a machine non-interactively on a simulated clock.
    #[command(visible_alias = "sim")]
    Simulate(SimulateArgs),
    /// List a path to every reachable configuration, or every simple path.
    Paths(PathsArgs),
    /// List the records of a snapshot store and find stale ones.
    Snapshots(SnapshotsArgs),
    /// Print version and build information.
    Info(InfoArgs),
}

fn dispatch(ui: Ui, command: &Command) -> Result<u8> {
    match command {
        Command::Validate(args) => commands::validate::run(ui, args),
        Command::Inspect(args) => commands::inspect::run(ui, args),
        Command::Diagram(args) => commands::diagram::run(ui, args),
        Command::Docs(args) => commands::docs::run(ui, args),
        Command::Simulate(args) => commands::simulate::run(ui, args),
        Command::Paths(args) => commands::paths::run(ui, args),
        Command::Snapshots(args) => commands::snapshots::run(ui, args),
        Command::Info(args) => commands::info::run(ui, args),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let ui = Ui::detect(cli.plain, cli.no_color, cli.verbose);
    match dispatch(ui, &cli.command) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("xsm: error: {err:#}");
            ExitCode::from(output::exit_code_of(&err))
        }
    }
}
