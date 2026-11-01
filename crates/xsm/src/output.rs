//! Exit codes, the error type commands use to pick one, and the `--json` printer.

use std::fmt;

use anyhow::Result;
use serde::Serialize;

/// Success.
pub(crate) const EXIT_OK: u8 = 0;
/// The command ran but found problems (validation failure, stale snapshots, ...).
pub(crate) const EXIT_FINDINGS: u8 = 1;
/// Usage error or refused input (bad flag value, unreadable store, malformed script).
pub(crate) const EXIT_USAGE: u8 = 2;

/// An error that carries the exit code the process should end with.
#[derive(Debug)]
pub(crate) struct CliError {
    /// Process exit code.
    pub(crate) code: u8,
    /// Message printed on stderr, without the `xsm: error:` prefix.
    pub(crate) message: String,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// An error that ends the process with [`EXIT_USAGE`].
pub(crate) fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CliError { code: EXIT_USAGE, message: message.into() })
}

/// An error that ends the process with [`EXIT_FINDINGS`].
pub(crate) fn failure(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CliError { code: EXIT_FINDINGS, message: message.into() })
}

/// Exit code an error maps to: the carried one, or [`EXIT_FINDINGS`] for anything else.
pub(crate) fn exit_code_of(err: &anyhow::Error) -> u8 {
    err.downcast_ref::<CliError>().map_or(EXIT_FINDINGS, |e| e.code)
}

/// Pretty-print a value as JSON on stdout (two-space indent, trailing newline).
pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Map a "did everything pass" flag to an exit code.
pub(crate) fn exit_for(ok: bool) -> u8 {
    if ok { EXIT_OK } else { EXIT_FINDINGS }
}
