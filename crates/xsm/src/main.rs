//! `xsm` — the command-line companion of xstate-statemachine.
//!
//! Phase 0 ships the binary skeleton only; `validate`, `inspect`, `simulate`, `diagram`,
//! `paths`, `coverage`, `snapshots`, `dlq` and the other subcommands arrive with the engine.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout)] // a CLI prints to stdout by design

use clap::{Parser, Subcommand};

/// Statecharts for Rust: validate, inspect, simulate and operate XState machines.
#[derive(Debug, Parser)]
#[command(name = "xsm", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print version and build information.
    Info,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Info => {
            println!("xsm {}", env!("CARGO_PKG_VERSION"));
            println!("snapshot layout v{}", xstate_core::SNAPSHOT_VERSION);
        }
    }
}
