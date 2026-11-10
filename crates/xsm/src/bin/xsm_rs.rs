//! `xsm-rs` executable entry point (unambiguous alias for environments with Python `xsm`).

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    xstate_statemachine_cli::run()
}
