//! Typed model of the conformance fixtures in `conformance/cases/` and a loader for them.
//!
//! A case pairs an XState JSON machine with stubbed logic, a step script and the behaviour
//! observed by two oracles (XState v5 and the Python library). See `conformance/README.md`
//! for the format and ADR-0001/ADR-0003 for how the oracles are used. Phase 1 adds the runner
//! that drives the Rust engine through each case.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error raised while loading fixtures.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// A directory or file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// Offending path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A fixture did not match the case model.
    #[error("invalid case {path}: {source}")]
    Parse {
        /// Offending path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
}

/// One conformance scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Unique identifier in `snake_case`.
    pub name: String,
    /// What the case pins down.
    pub description: String,
    /// Free-form classification tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The machine configuration in XState v5 JSON.
    pub machine: Value,
    /// Stubbed guards, services and delays.
    #[serde(default)]
    pub logic: Logic,
    /// Script driven against the started machine.
    pub steps: Vec<Step>,
    /// Present only when the two oracles disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divergence: Option<Divergence>,
    /// Oracle observations; generated, never hand-written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expected>,
}

/// Stub implementations for the names a machine references.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logic {
    /// Constant guard results; unlisted guards evaluate to `true`.
    #[serde(default)]
    pub guards: BTreeMap<String, bool>,
    /// Service outcomes; unlisted services complete with `null`.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceStub>,
    /// Named delays in milliseconds.
    #[serde(default)]
    pub delays: BTreeMap<String, f64>,
}

/// Outcome of a stubbed service: either a result or an error message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStub {
    /// Value delivered through `onDone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Message delivered through `onError`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One scripted action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Step {
    /// Send an event: a bare type string or an object with `type` plus payload fields.
    #[serde(rename = "send")]
    Send(Value),
    /// Advance the simulated clock by this many milliseconds.
    #[serde(rename = "advance_ms")]
    AdvanceMs(f64),
}

/// Documentation of a disagreement between the oracles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    /// What the Python engine does.
    pub python: String,
    /// What SCXML / XState does.
    pub spec: String,
    /// Which oracle the Rust engine follows for this case.
    pub rust_follows: String,
}

/// Observations from both oracles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// XState v5 observations (semantic reference).
    pub xstate: Expectation,
    /// Python observations plus snapshot-format vectors.
    pub python: Expectation,
}

/// What one oracle observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expectation {
    /// Oracle identity.
    pub oracle: Oracle,
    /// Observation right after `start()`.
    pub initial: Observation,
    /// One observation per step, in order.
    pub steps: Vec<Observation>,
    /// Python only: 16 hex chars of the machine structure hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_hash: Option<String>,
    /// Python only: end-of-run snapshot (layout v4) with volatile fields zeroed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

/// Name and version of an oracle implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Oracle {
    /// `xstate` or `xstate-statemachine`.
    pub name: String,
    /// Package version the expectations were generated with.
    pub version: String,
}

/// The machine as seen after a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Active leaf state ids, sorted.
    pub state_ids: Vec<String>,
    /// Context after the step.
    pub context: Value,
    /// User actions executed since the previous observation, in order.
    pub actions: Vec<String>,
    /// `running`, `done`, `error` or `stopped`.
    pub status: String,
    /// Machine output, present once `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Error message, present once `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Path of the `conformance/cases` directory inside this repository.
#[must_use]
pub fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases")
}

/// Load one case file.
///
/// # Errors
/// Returns [`LoadError`] when the file cannot be read or does not match the case model.
pub fn load_case(path: &Path) -> Result<Case, LoadError> {
    let text = fs::read_to_string(path)
        .map_err(|source| LoadError::Io { path: path.to_path_buf(), source })?;
    serde_json::from_str(&text)
        .map_err(|source| LoadError::Parse { path: path.to_path_buf(), source })
}

/// Load every `*.json` case in a directory, sorted by file name.
///
/// # Errors
/// Returns the first [`LoadError`] encountered.
pub fn load_cases(dir: &Path) -> Result<Vec<Case>, LoadError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|source| LoadError::Io { path: dir.to_path_buf(), source })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths.iter().map(|p| load_case(p)).collect()
}
