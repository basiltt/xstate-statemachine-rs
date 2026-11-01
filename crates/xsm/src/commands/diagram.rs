//! `xsm diagram`: Mermaid, `PlantUML` or JSON export of a machine.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use xstate_core::Machine;

use super::analysis::load_machine;
use super::model::file_stem_for;
use crate::output;
use crate::ui::Ui;

/// Export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// Mermaid `stateDiagram-v2`.
    Mermaid,
    /// `PlantUML`.
    Plantuml,
    /// The XState JSON document the machine was built from.
    Json,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Self::Mermaid => "mmd",
            Self::Plantuml => "puml",
            Self::Json => "json",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Mermaid => "mermaid",
            Self::Plantuml => "plantuml",
            Self::Json => "json",
        }
    }

    fn render(self, machine: &Machine) -> String {
        match self {
            Self::Mermaid => machine.to_mermaid(),
            Self::Plantuml => machine.to_plantuml(),
            Self::Json => machine.to_json_string(),
        }
    }
}

/// Arguments of `diagram`.
#[derive(Debug, clap::Args)]
pub(crate) struct DiagramArgs {
    /// Machine JSON file.
    file: PathBuf,
    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Mermaid)]
    format: Format,
    /// Write to this file, or into this directory as `<machine id>.<ext>`, instead of stdout.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
}

/// Where `-o` points: an existing directory, a path written with a trailing separator, or a
/// file.
fn resolve_target(output: &Path, machine: &Machine, format: Format) -> PathBuf {
    let text = output.to_string_lossy();
    let is_dir = output.is_dir() || text.ends_with('/') || text.ends_with('\\');
    if is_dir {
        output.join(format!("{}.{}", file_stem_for(machine.id()), format.extension()))
    } else {
        output.to_path_buf()
    }
}

/// Run `diagram`.
pub(crate) fn run(ui: Ui, args: &DiagramArgs) -> Result<u8> {
    let machine = load_machine(ui, &args.file)?;
    let mut text = args.format.render(&machine);
    if let Some(output) = &args.output {
        let target = resolve_target(output, &machine, args.format);
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        std::fs::write(&target, text)
            .with_context(|| format!("cannot write {}", target.display()))?;
        println!("Wrote {} diagram: {}", args.format.name(), target.display());
        return Ok(output::EXIT_OK);
    }
    println!("{text}");
    Ok(output::EXIT_OK)
}
