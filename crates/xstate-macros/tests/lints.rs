//! A user crate with `#![deny(warnings)]` and `#![warn(clippy::pedantic)]` must compile the
//! macros' generated code without a single warning: run `cargo clippy` on `tests/lints`.

use std::path::Path;
use std::process::Command;

#[test]
fn generated_code_is_warning_free_under_deny_warnings_and_pedantic() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    if Command::new(&cargo).arg("--version").output().is_err() {
        eprintln!("skipping: `cargo` is not on PATH");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lints");
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/lints");
    // `-D warnings` escalates every rustc warning (including `#![warn(clippy::pedantic)]`) to an
    // error; `--all-targets` covers the lib and any test/example the crate defines.
    let output = Command::new(&cargo)
        .args(["clippy", "--quiet", "--all-targets", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .args(["--", "-D", "warnings"])
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTFLAGS")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => panic!("running cargo clippy failed: {e}"),
    };
    assert!(
        output.status.success(),
        "generated code is not warning-free under `deny(warnings)` + `clippy::pedantic`:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
