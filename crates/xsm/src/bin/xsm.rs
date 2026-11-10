//! `xsm` executable entry point.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    xstate_statemachine_cli::run()
}
