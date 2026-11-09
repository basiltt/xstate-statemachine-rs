//! A crate that depends on the facade under a renamed dependency (`myfacade = { package =
//! "xstate-statemachine", .. }`) must compile: the generated code resolves `xstate_core` and
//! `serde_json` through the renamed facade, not through a literal crate name.

use std::path::Path;
use std::process::Command;

#[test]
fn renamed_facade_dependency_compiles() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    if Command::new(&cargo).arg("--version").output().is_err() {
        eprintln!("skipping: `cargo` is not on PATH");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/renamed");
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/renamed");
    let output = Command::new(&cargo)
        .args(["check", "--quiet", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTFLAGS")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => panic!("running cargo failed: {e}"),
    };
    assert!(
        output.status.success(),
        "renamed-facade crate failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
