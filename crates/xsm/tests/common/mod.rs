//! Helpers shared by the integration tests.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

/// The `xsm` binary with colour-related environment cleared.
pub(crate) fn xsm() -> Command {
    let mut cmd = cargo_bin_cmd!("xsm");
    cmd.env_remove("NO_COLOR").env_remove("XSM_NO_COLOR");
    cmd
}

/// A file under `tests/fixtures`.
pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

/// Run `cmd` and parse its stdout as JSON.
pub(crate) fn run_json(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("stdout is not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout))
    })
}
