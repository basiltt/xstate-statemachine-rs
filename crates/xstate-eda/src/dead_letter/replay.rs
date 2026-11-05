//! Replay a dead-lettered envelope through a dispatcher (Python `replay_dead_letter`, used by
//! `xsm dlq replay`).

use serde_json::{Map, Value};
use xstate_core::Machine;

use super::{DeadLetter, DeadLetterStore};
use crate::dispatcher::{EnvelopeHandler, InboundDispatcher};
use crate::envelope::{ATTEMPT_EXTENSION, Envelope};
use crate::error::{EdaError, Result};
use crate::py::{repr_opt, repr_value, wall_now};

/// Options of [`replay_dead_letter`].
#[derive(Debug, Clone)]
pub struct ReplayOptions<'a> {
    /// Why (required, audited).
    pub reason: &'a str,
    /// Only report (default `true`).
    pub dry_run: bool,
    /// Replay despite a machine hash / version mismatch.
    pub force: bool,
    /// Who (default the login name).
    pub actor: Option<&'a str>,
}

impl<'a> ReplayOptions<'a> {
    /// A dry run with `reason`.
    #[must_use]
    pub fn new(reason: &'a str) -> Self {
        Self { reason, dry_run: true, force: false, actor: None }
    }
}

/// What [`replay_dead_letter`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    /// The dead letter's id (the envelope id).
    pub record_id: String,
    /// `true` when nothing was sent.
    pub dry_run: bool,
    /// `would_replay` (dry run) or the dispatcher outcome: `processed` / `duplicate` resolve
    /// the record; anything else (`retry`, `dead_lettered:<reason>`) leaves it open.
    pub outcome: String,
    /// Machine mismatches overridden with `force`.
    pub warnings: Vec<String>,
}

/// Re-send a dead-lettered envelope through `dispatcher`.
///
/// * Reuses the envelope id (attempt counter reset) so an inbox dedups a double replay.
/// * Refuses when the record's `machine_hash` / `machine_version` differ from the machine the
///   dispatcher would use, unless `force`.
/// * Refuses when the stored envelope data holds the redaction sentinel (`"***"`) as a leaf:
///   dead letters are redacted at rest, so replaying one would send masked values; unless
///   `force`.
/// * A dry run (the default) only reports; otherwise the record is marked resolved on success
///   and the action is audited.
///
/// # Errors
/// [`EdaError::ReplayRefused`] for a missing reason, an unknown id, a chart-state record or a
/// mismatch; store errors; [`EdaError::Envelope`] when the stored envelope no longer decodes.
pub fn replay_dead_letter<H: EnvelopeHandler>(
    dead_letters: &dyn DeadLetterStore,
    record_id: &str,
    dispatcher: &InboundDispatcher<H>,
    options: &ReplayOptions<'_>,
) -> Result<ReplayResult> {
    if options.reason.trim().is_empty() {
        return Err(EdaError::ReplayRefused("a replay needs a --reason".into()));
    }
    let Some(rec) = dead_letters.get(record_id)? else {
        return Err(EdaError::ReplayRefused(format!(
            "no dead letter with id {}",
            crate::py::repr_str(record_id)
        )));
    };
    let Some(raw) = rec.envelope.as_ref().filter(|e| e.as_object().is_some_and(|m| !m.is_empty()))
    else {
        return Err(EdaError::ReplayRefused(
            "this dead letter was captured from a chart state, not an envelope; restore its \
             snapshot instead"
                .into(),
        ));
    };
    if !options.force && holds_redacted(raw.get("data").unwrap_or(&Value::Null)) {
        return Err(EdaError::ReplayRefused(
            "the stored envelope data was redacted at capture ('***'); replaying it would send masked values (use --force to replay anyway)"
                .into(),
        ));
    }
    let mut env = Envelope::from_dict(raw)?;
    env.extensions.shift_remove(ATTEMPT_EXTENSION);
    let warnings = mismatches(&rec, dispatcher.machine_for(&env.ty).as_ref());
    if !warnings.is_empty() && !options.force {
        return Err(EdaError::ReplayRefused(format!(
            "{} (use --force to replay anyway)",
            warnings.join("; ")
        )));
    }
    if options.dry_run {
        return Ok(ReplayResult {
            record_id: rec.id,
            dry_run: true,
            outcome: "would_replay".into(),
            warnings,
        });
    }
    let res = dispatcher.handle(&env, rec.topic.as_deref());
    let outcome = res.outcomes.last().map_or_else(|| "unknown".to_owned(), |(_, o)| o.clone());
    if outcome == "processed" || outcome == "duplicate" {
        dead_letters.mark_resolved(&rec.id, wall_now())?;
    }
    let mut detail = Map::new();
    detail.insert("outcome".into(), Value::from(outcome.clone()));
    detail.insert("forced".into(), Value::from(options.force && !warnings.is_empty()));
    dead_letters.audit(
        "replay",
        Some(&rec.id),
        options.reason,
        &Value::Object(detail),
        options.actor,
    )?;
    Ok(ReplayResult { record_id: rec.id, dry_run: false, outcome, warnings })
}

/// The redaction sentinel the capture side writes in place of a secret.
const REDACTED: &str = "***";

/// Whether `value` holds the redaction sentinel as a leaf (iterative, like Python's walk).
fn holds_redacted(value: &Value) -> bool {
    let mut stack = vec![value];
    while let Some(item) = stack.pop() {
        match item {
            Value::Object(m) => stack.extend(m.values()),
            Value::Array(a) => stack.extend(a),
            Value::String(s) if s == REDACTED => return true,
            _ => {}
        }
    }
    false
}

fn mismatches(rec: &DeadLetter, machine: Option<&Machine>) -> Vec<String> {
    let Some(machine) = machine else {
        let ty = rec.event.get("type").unwrap_or(&Value::Null);
        return vec![format!("no machine handles type {} now", repr_value(ty))];
    };
    let mut out = Vec::new();
    let hash = machine.structure_hash();
    if let Some(captured) = rec.machine_hash.as_deref().filter(|h| !h.is_empty())
        && captured != hash
    {
        out.push(format!("machine structure changed since capture ({captured} != {hash})"));
    }
    if let Some(captured) = rec.machine_version.as_deref().filter(|v| !v.is_empty())
        && Some(captured) != machine.version()
    {
        out.push(format!(
            "machine version changed since capture ({} != {})",
            repr_opt(Some(captured)),
            repr_opt(machine.version())
        ));
    }
    out
}
