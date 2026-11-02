//! A crate that depends only on the `xstate-statemachine` facade must compile the macros'
//! output: the generated code resolves `xstate_core` and `serde_json` through the facade.

use std::path::Path;
use std::process::Command;

#[test]
fn facade_only_crate_compiles() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    if Command::new(&cargo).arg("--version").output().is_err() {
        eprintln!("skipping: `cargo` is not on PATH");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/facade-only");
    // A separate target dir: the test itself runs under a held build-directory lock.
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/facade-only");
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
        "facade-only crate failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
