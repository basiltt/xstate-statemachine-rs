//! Telemetry hygiene: what may become a metric label (port of Python `_hygiene.py`).
//!
//! A metrics backend is a public, long-retention, cardinality-priced sink. Three rules:
//!
//! 1. Label *values* come from the chart, not from traffic: an event type the machine does not
//!    declare becomes [`UNKNOWN`], so a fuzzer cannot mint series ([`event_label`]).
//! 2. Every label dimension is additionally capped ([`LabelGuard`]); the overflow bucket is
//!    [`OTHER`].
//! 3. Payloads, context and instance keys are never labels.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};

use xstate_core::Machine;
use xstate_core::event::names;

/// Fallback for a value outside the chart's allow-list.
pub const UNKNOWN: &str = "unknown";
/// Overflow bucket once a label dimension reaches its cap.
pub const OTHER: &str = "other";
/// Label of a transition with no event trigger (`always`).
pub const ALWAYS: &str = "always";

/// Default cap on distinct values per label dimension (as in Python).
pub const DEFAULT_MAX_LABEL_VALUES: usize = 100;

/// Engine event families collapsed to their family so per-invocation ids cannot leak in.
const FAMILIES: [&str; 3] = ["after", "done", "error"];

/// Map an event type to an allow-listed label value.
///
/// Types declared by the chart (`Machine::declared_events`) pass through; `xstate.init` is kept;
/// the engine's own families (`after.*`, `done.*`, `error.*`, `xstate.*`, with or without the
/// `xstate.` prefix) collapse to the family name; anything else is [`UNKNOWN`].
#[must_use]
pub fn event_label(machine: &Machine, event_type: &str) -> String {
    if event_type.is_empty() {
        return ALWAYS.to_owned();
    }
    if event_type == names::INIT {
        return names::INIT.to_owned();
    }
    if machine.declared_events().binary_search_by(|d| d.as_str().cmp(event_type)).is_ok() {
        return event_type.to_owned();
    }
    let bare = event_type.strip_prefix("xstate.").unwrap_or(event_type);
    for family in FAMILIES {
        if bare.strip_prefix(family).is_some_and(|rest| rest.starts_with('.')) {
            return family.to_owned();
        }
    }
    if event_type.starts_with("xstate.") {
        return "xstate".to_owned();
    }
    UNKNOWN.to_owned()
}

/// Caps the number of distinct values per label dimension.
///
/// Thread-safe. The first `max_label_values` values seen per dimension are kept verbatim; later
/// ones become [`OTHER`]. The fallbacks [`UNKNOWN`] and [`OTHER`] themselves never consume a
/// slot, so a flood of undeclared events cannot crowd out declared ones (a deliberate
/// tightening of the Python guard, which counts `unknown` as a value).
#[derive(Debug)]
pub struct LabelGuard {
    max_label_values: NonZeroUsize,
    seen: Mutex<HashMap<&'static str, HashSet<String>>>,
}

impl LabelGuard {
    /// A guard allowing `max_label_values` distinct values per dimension.
    #[must_use]
    pub fn new(max_label_values: NonZeroUsize) -> Self {
        Self { max_label_values, seen: Mutex::new(HashMap::new()) }
    }

    /// The configured cap.
    #[must_use]
    pub fn max_label_values(&self) -> usize {
        self.max_label_values.get()
    }

    /// The value to record for `value` in `dimension`: `value` itself while the dimension has
    /// room (or already knows it), [`OTHER`] afterwards.
    pub fn label(&self, dimension: &'static str, value: &str) -> String {
        if value == UNKNOWN || value == OTHER {
            return value.to_owned();
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let values = seen.entry(dimension).or_default();
        if values.contains(value) {
            return value.to_owned();
        }
        if values.len() >= self.max_label_values.get() {
            return OTHER.to_owned();
        }
        values.insert(value.to_owned());
        value.to_owned()
    }
}

impl Default for LabelGuard {
    fn default() -> Self {
        // 100 is non-zero; `MIN` is the unreachable fallback and keeps this panic-free.
        Self::new(NonZeroUsize::new(DEFAULT_MAX_LABEL_VALUES).unwrap_or(NonZeroUsize::MIN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(n: usize) -> LabelGuard {
        LabelGuard::new(NonZeroUsize::new(n).expect("non-zero"))
    }

    #[test]
    fn keeps_first_values_then_collapses_to_other() {
        let g = guard(2);
        assert_eq!(g.label("d", "a"), "a");
        assert_eq!(g.label("d", "b"), "b");
        assert_eq!(g.label("d", "c"), OTHER);
        assert_eq!(g.label("d", "a"), "a");
    }

    #[test]
    fn dimensions_are_independent() {
        let g = guard(1);
        assert_eq!(g.label("x", "a"), "a");
        assert_eq!(g.label("y", "b"), "b");
        assert_eq!(g.label("x", "b"), OTHER);
    }

    #[test]
    fn fallbacks_do_not_consume_slots() {
        let g = guard(1);
        assert_eq!(g.label("d", UNKNOWN), UNKNOWN);
        assert_eq!(g.label("d", OTHER), OTHER);
        assert_eq!(g.label("d", "a"), "a");
    }

    #[test]
    fn event_label_allow_lists_against_the_chart() {
        let m = Machine::from_json(
            r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b"}},"b":{}}}"#,
        )
        .expect("machine");
        assert_eq!(event_label(&m, "GO"), "GO");
        assert_eq!(event_label(&m, "FUZZ-123"), UNKNOWN);
        assert_eq!(event_label(&m, ""), ALWAYS);
        assert_eq!(event_label(&m, "xstate.init"), "xstate.init");
        assert_eq!(event_label(&m, "xstate.after.500.m.a"), "after");
        assert_eq!(event_label(&m, "xstate.done.actor.fetch"), "done");
        assert_eq!(event_label(&m, "xstate.error.actor.fetch"), "error");
        assert_eq!(event_label(&m, "xstate.other"), "xstate");
        assert_eq!(event_label(&m, "afterwards"), UNKNOWN);
    }
}
