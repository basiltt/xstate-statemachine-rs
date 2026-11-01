//! `xsm info`: version, crate versions, snapshot layout and capabilities.

use anyhow::Result;
use serde_json::{Value, json};

use crate::output::{self, print_json};
use crate::ui::{Role, Ui, print_lines};

/// Workspace crates released in lockstep with the binary (they share one version).
const WORKSPACE_CRATES: [&str; 4] = ["xsm", "xstate-core", "xstate-persistence", "xstate-inspect"];

/// Commands this build ships.
const COMMANDS: [&str; 8] =
    ["validate", "inspect", "diagram", "docs", "simulate", "paths", "snapshots", "info"];

/// Features that are not available yet (documented TODOs).
const PLANNED: [&str; 3] = ["inspect --live", "simulate --record", "simulate (interactive)"];

/// Arguments of `info`.
#[derive(Debug, clap::Args)]
pub(crate) struct InfoArgs {
    /// Emit JSON instead of text.
    #[arg(long)]
    json: bool,
}

fn payload() -> Value {
    let version = env!("CARGO_PKG_VERSION");
    let crates: serde_json::Map<String, Value> =
        WORKSPACE_CRATES.iter().map(|name| ((*name).to_owned(), json!(version))).collect();
    json!({
        "name": "xsm",
        "version": version,
        "crates": crates,
        "snapshot_layout": xstate_core::SNAPSHOT_VERSION,
        "max_input_bytes": xstate_core::DEFAULT_MAX_INPUT_BYTES,
        "default_max_iterations": xstate_core::DEFAULT_MAX_ITERATIONS,
        "features": {
            "commands": COMMANDS,
            "stores": ["sqlite", "file"],
            "diagram_formats": ["mermaid", "plantuml", "json"],
            "planned": PLANNED,
        },
        "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "repository": env!("CARGO_PKG_REPOSITORY"),
    })
}

/// Run `info`.
pub(crate) fn run(ui: Ui, args: &InfoArgs) -> Result<u8> {
    let data = payload();
    if args.json {
        print_json(&data)?;
        return Ok(output::EXIT_OK);
    }
    let version = env!("CARGO_PKG_VERSION");
    let crates: Vec<String> = WORKSPACE_CRATES.iter().map(|c| format!("{c} {version}")).collect();
    let rows = vec![
        ("Version", version.to_owned()),
        ("Crates", crates.join(", ")),
        ("Snapshot layout", format!("v{}", xstate_core::SNAPSHOT_VERSION)),
        ("Commands", COMMANDS.join(", ")),
        ("Stores", "sqlite, file".to_owned()),
        ("Diagrams", "mermaid, plantuml, json".to_owned()),
        ("Planned", PLANNED.join(", ")),
        ("Platform", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)),
    ];
    println!();
    println!("  {}", ui.paint(Role::Title, "xsm - statecharts for Rust"));
    println!();
    print_lines(&ui.key_values(&rows).into_iter().map(|l| format!("  {l}")).collect::<Vec<_>>());
    println!();
    Ok(output::EXIT_OK)
}
