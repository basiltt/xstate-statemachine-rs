//! `xsm codegen`: Generate zero-cost native Rust enum state machines from XState JSON.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use xstate_core::codegen::{CodegenOptions, generate_rust_code};

use super::analysis::load_machine;
use super::model::file_stem_for;
use crate::ui::{Role, Ui};

/// Arguments of `codegen`.
#[derive(Debug, clap::Args)]
pub(crate) struct CodegenArgs {
    /// Machine JSON file to read.
    pub file: PathBuf,
    /// Write to this file (or directory as `<machine_id>.rs`) instead of stdout.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Base name for generated types (e.g. "Order" -> `OrderState`, `OrderEvent`, `OrderMachine`).
    #[arg(short, long)]
    pub name: Option<String>,
    /// Check that the output file exists and matches the generated code without writing (drift gate).
    #[arg(long)]
    pub check: bool,
    /// Include an embedded `#[cfg(test)]` unit test module.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub tests: bool,
    /// Visibility for the generated types (e.g. "pub", "pub(crate)").
    #[arg(long, default_value = "pub")]
    pub vis: String,
    /// Crate path to reach `xstate_core` (defaults to "`xstate_core`").
    #[arg(long, default_value = "xstate_core")]
    pub crate_path: String,
}

fn resolve_target(output: &Path, machine_id: &str) -> PathBuf {
    let text = output.to_string_lossy();
    let is_dir = output.is_dir() || text.ends_with('/') || text.ends_with('\\');
    if is_dir {
        output.join(format!("{}.rs", file_stem_for(machine_id)))
    } else {
        output.to_path_buf()
    }
}

/// Run `codegen`.
pub(crate) fn run(ui: Ui, args: &CodegenArgs) -> Result<u8> {
    let machine = load_machine(ui, &args.file)?;
    let options = CodegenOptions {
        machine_name: args.name.clone(),
        visibility: args.vis.clone(),
        include_tests: args.tests,
        include_json_export: true,
        crate_path: args.crate_path.clone(),
    };

    let generated = generate_rust_code(&machine, &options);
    let target_path = args.output.as_ref().map(|p| resolve_target(p, machine.id()));

    if args.check {
        let Some(target) = target_path else {
            bail!("--check requires -o/--output specifying the target file to verify");
        };

        if !target.exists() {
            eprintln!(
                "{} Target file `{}` does not exist.",
                ui.paint(Role::Err, "FAIL:"),
                target.display()
            );
            return Ok(1);
        }

        let existing = std::fs::read_to_string(&target)
            .with_context(|| format!("failed to read `{}`", target.display()))?;

        // Normalize line endings for cross-platform comparison
        let norm_existing = existing.replace("\r\n", "\n");
        let norm_generated = generated.replace("\r\n", "\n");

        if norm_existing.trim() != norm_generated.trim() {
            eprintln!(
                "{} Drift detected: `{}` is out-of-sync with `{}`.",
                ui.paint(Role::Err, "FAIL:"),
                target.display(),
                args.file.display()
            );
            return Ok(1);
        }

        println!(
            "{} `{}` is up-to-date with `{}`.",
            ui.paint(Role::Ok, "OK:"),
            target.display(),
            args.file.display()
        );
        return Ok(0);
    }

    if let Some(target) = target_path {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory `{}`", parent.display()))?;
        }
        std::fs::write(&target, generated)
            .with_context(|| format!("failed to write `{}`", target.display()))?;
        ui.verbose(&format!("Generated `{}` from `{}`", target.display(), args.file.display()));
    } else {
        print!("{generated}");
    }

    Ok(0)
}
