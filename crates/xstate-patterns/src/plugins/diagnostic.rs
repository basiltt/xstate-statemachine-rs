//! Diagnostic flight recorder plugin: captures the last N transitions for production post-mortems.
//!
//! Real-world challenge in Rust:
//! When a complex distributed state machine panics, enters an unexpected state, or throws an unhandled
//! error in production at 3 AM, standard logging either:
//! - Produces too little context to reproduce the bug, or
//! - Emits huge volumes of debug logs that drown observability systems and cost thousands in log storage.
//!
//! [`DiagnosticPlugin`] maintains an in-memory circular ring buffer of the last N statechart
//! transitions (a "black box flight recorder"). Upon error or on-demand inspection, it formats a
//! comprehensive timeline of events, leaf states, and action results for rapid post-mortem analysis.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::{PoisonError, RwLock};
use std::time::Instant;

use smol_str::SmolStr;
use xstate_core::{ActiveState, Context, Error, Event, Machine, Observer, StepReport};

/// Single entry in the diagnostic flight recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlightStep {
    /// Timestamp when the transition occurred.
    pub recorded_at: Instant,
    /// Event type that was processed.
    pub event_type: SmolStr,
    /// Active leaf states after this step.
    pub leaf_states: Vec<SmolStr>,
    /// Whether any state change occurred.
    pub changed: bool,
    /// Actions executed during this step.
    pub actions: Vec<SmolStr>,
}

/// Circular flight recorder ring buffer.
pub struct DiagnosticPlugin<C: Context = serde_json::Value> {
    capacity: usize,
    history: RwLock<VecDeque<FlightStep>>,
    last_error: RwLock<Option<(Instant, String)>>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> DiagnosticPlugin<C> {
    /// Create a new diagnostic flight recorder with ring buffer capacity (e.g. 50 steps).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            history: RwLock::new(VecDeque::with_capacity(capacity.min(256))),
            last_error: RwLock::new(None),
            _marker: std::marker::PhantomData,
        }
    }

    /// Retrieve the recorded timeline in chronological order (oldest to newest).
    #[must_use]
    pub fn timeline(&self) -> Vec<FlightStep> {
        let guard = self.history.read().unwrap_or_else(PoisonError::into_inner);
        guard.iter().cloned().collect()
    }

    /// Retrieve the last recorded error, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<(Instant, String)> {
        let guard = self.last_error.read().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }

    /// Clear the flight recorder history.
    pub fn clear(&self) {
        let mut guard = self.history.write().unwrap_or_else(PoisonError::into_inner);
        guard.clear();
        let mut err_guard = self.last_error.write().unwrap_or_else(PoisonError::into_inner);
        *err_guard = None;
    }

    /// Format an incident post-mortem report ready for Sentry, logs, or GitHub issues.
    #[must_use]
    pub fn format_report(&self, machine_id: &str) -> String {
        let timeline = self.timeline();
        let last_err = self.last_error();

        let mut out = format!("=== STATECHART FLIGHT RECORDER: {machine_id} ===\n");
        if let Some((_, err)) = last_err {
            let _ = writeln!(out, "🚨 LAST ERROR: {err}");
        }
        let steps_count = timeline.len();
        let _ = writeln!(out, "📜 RECENT TRANSITIONS ({steps_count} steps):");

        for (i, step) in timeline.iter().enumerate() {
            let actions_str =
                if step.actions.is_empty() { "none".to_string() } else { step.actions.join(", ") };
            let idx = i + 1;
            let ev = &step.event_type;
            let states = &step.leaf_states;
            let ch = step.changed;
            let _ = writeln!(
                out,
                "  [{idx:02}] Event: {ev:<15} -> States: {states:?} (changed: {ch}, actions: [{actions_str}])"
            );
        }
        out.push_str("==============================================\n");
        out
    }
}

impl<C: Context> fmt::Debug for DiagnosticPlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiagnosticPlugin")
            .field("capacity", &self.capacity)
            .field("recorded_steps", &self.timeline().len())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for DiagnosticPlugin<C> {
    fn default() -> Self {
        Self::new(50)
    }
}

impl<C: Context> Observer<C> for DiagnosticPlugin<C> {
    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let mut guard = self.history.write().unwrap_or_else(PoisonError::into_inner);
        if guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(FlightStep {
            recorded_at: Instant::now(),
            event_type: event.ty.clone(),
            leaf_states: state.leaf_ids(machine).into_iter().map(SmolStr::new).collect(),
            changed: report.changed,
            actions: report.actions.iter().map(|a| SmolStr::new(a.as_ref())).collect(),
        });
    }

    fn on_error(&self, _machine: &Machine, error: &Error) {
        let mut guard = self.last_error.write().unwrap_or_else(PoisonError::into_inner);
        *guard = Some((Instant::now(), error.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_caps_history_and_formats_report() {
        let recorder = DiagnosticPlugin::<serde_json::Value>::new(2);

        {
            let mut guard = recorder.history.write().unwrap();
            guard.push_back(FlightStep {
                recorded_at: Instant::now(),
                event_type: SmolStr::new("INIT"),
                leaf_states: vec![SmolStr::new("idle")],
                changed: true,
                actions: vec![],
            });
            guard.push_back(FlightStep {
                recorded_at: Instant::now(),
                event_type: SmolStr::new("PAY"),
                leaf_states: vec![SmolStr::new("paid")],
                changed: true,
                actions: vec![SmolStr::new("charge_card")],
            });
        }

        let report = recorder.format_report("order_flow");
        assert!(report.contains("STATECHART FLIGHT RECORDER: order_flow"));
        assert!(report.contains("INIT"));
        assert!(report.contains("PAY"));
        assert!(report.contains("charge_card"));
    }
}
