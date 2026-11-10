//! Watchdog plugin: monitors statechart execution for hung states, deadlocks, and SLA breaches.
//!
//! Real-world challenge in Rust:
//! In Tokio actors and async worker loops, async tasks can silently hang indefinitely when an
//! external HTTP request lacks a timeout, a database lock deadlocks, or an unbounded channel never sends.
//! Tokio does not provide per-actor watchdogs. Diagnosing which actor or state is stalled requires
//! tedious manual instrumentation or wrapping every future in `tokio::time::timeout`.
//!
//! [`WatchdogPlugin`] monitors active states and measures time elapsed since entry. If an active
//! state exceeds its SLA deadline, it triggers an alert hook, records diagnostic telemetry, and
//! generates an recovery event (`"xstate.watchdog.timeout"`) to safely transition the actor to
//! an error or fallback state.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use smol_str::SmolStr;
use xstate_core::{ActiveState, Context, Event, Machine, Observer, StepReport};

/// Callback invoked when a state SLA expires.
pub type WatchdogAlertHandler = Arc<dyn Fn(&WatchdogAlert) + Send + Sync>;

/// Default event type emitted when a state SLA expires.
pub const WATCHDOG_TIMEOUT_EVENT: &str = "xstate.watchdog.timeout";

/// Alert generated when an active state exceeds its configured SLA duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogAlert {
    /// Identifier of the machine that stalled.
    pub machine_id: SmolStr,
    /// Currently active leaf states.
    pub active_states: Vec<SmolStr>,
    /// Duration elapsed since entering the state.
    pub elapsed: Duration,
    /// Configured threshold that was breached.
    pub threshold: Duration,
}

#[derive(Debug, Clone)]
struct ActiveStateRecord {
    machine_id: SmolStr,
    active_states: Vec<SmolStr>,
    entered_at: Instant,
}

/// Watches for hung async tasks, deadlock states, or SLA duration breaches.
pub struct WatchdogPlugin<C: Context = serde_json::Value> {
    state_timeouts: HashMap<SmolStr, Duration>,
    default_timeout: Option<Duration>,
    current_state: RwLock<Option<ActiveStateRecord>>,
    alerts_triggered: AtomicU64,
    alert_handler: Option<WatchdogAlertHandler>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: Context> WatchdogPlugin<C> {
    /// Create a new watchdog with an optional default timeout for any non-final state.
    #[must_use]
    pub fn new(default_timeout: Option<Duration>) -> Self {
        Self {
            state_timeouts: HashMap::new(),
            default_timeout,
            current_state: RwLock::new(None),
            alerts_triggered: AtomicU64::new(0),
            alert_handler: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Set an SLA timeout for a specific state.
    #[must_use]
    pub fn with_state_timeout(mut self, state: impl Into<SmolStr>, timeout: Duration) -> Self {
        self.state_timeouts.insert(state.into(), timeout);
        self
    }

    /// Attach an alert handler callback.
    #[must_use]
    pub fn with_alert_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WatchdogAlert) + Send + Sync + 'static,
    {
        self.alert_handler = Some(Arc::new(handler));
        self
    }

    /// Check if the currently active state has exceeded its SLA threshold.
    ///
    /// If stalled, returns [`WatchdogAlert`], increments alert metrics, and invokes the alert callback.
    pub fn check(&self, now: Instant) -> Option<WatchdogAlert> {
        let guard = self.current_state.read().unwrap_or_else(PoisonError::into_inner);
        let record = guard.as_ref()?;

        let elapsed = now.saturating_duration_since(record.entered_at);

        // Find applicable threshold: check specific state timeouts first, then default
        let mut threshold = self.default_timeout;
        for s in &record.active_states {
            if let Some(t) = self.state_timeouts.get(s) {
                threshold = Some(*t);
                break;
            }
        }

        let threshold = threshold?;
        if elapsed >= threshold {
            self.alerts_triggered.fetch_add(1, Ordering::Relaxed);
            let alert = WatchdogAlert {
                machine_id: record.machine_id.clone(),
                active_states: record.active_states.clone(),
                elapsed,
                threshold,
            };
            if let Some(handler) = &self.alert_handler {
                handler(&alert);
            }
            Some(alert)
        } else {
            None
        }
    }

    /// Total alerts triggered by this watchdog.
    #[must_use]
    pub fn alerts_triggered(&self) -> u64 {
        self.alerts_triggered.load(Ordering::Relaxed)
    }

    /// Generate a recovery event to inject into the machine or actor mailbox.
    #[must_use]
    pub fn make_recovery_event(&self) -> Event {
        Event::new(WATCHDOG_TIMEOUT_EVENT)
    }
}

impl<C: Context> fmt::Debug for WatchdogPlugin<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchdogPlugin")
            .field("default_timeout", &self.default_timeout)
            .field("state_timeouts", &self.state_timeouts)
            .field("alerts_triggered", &self.alerts_triggered())
            .finish_non_exhaustive()
    }
}

impl<C: Context> Default for WatchdogPlugin<C> {
    fn default() -> Self {
        Self::new(Some(Duration::from_secs(30)))
    }
}

impl<C: Context> Observer<C> for WatchdogPlugin<C> {
    fn on_start(&self, machine: &Machine, state: &ActiveState<C>, _report: &StepReport) {
        let mut guard = self.current_state.write().unwrap_or_else(PoisonError::into_inner);
        *guard = Some(ActiveStateRecord {
            machine_id: SmolStr::new(machine.id()),
            active_states: state.leaf_ids(machine).into_iter().map(SmolStr::new).collect(),
            entered_at: Instant::now(),
        });
    }

    fn on_event(
        &self,
        machine: &Machine,
        _event: &Event,
        _report: &StepReport,
        state: &ActiveState<C>,
    ) {
        let mut guard = self.current_state.write().unwrap_or_else(PoisonError::into_inner);
        *guard = Some(ActiveStateRecord {
            machine_id: SmolStr::new(machine.id()),
            active_states: state.leaf_ids(machine).into_iter().map(SmolStr::new).collect(),
            entered_at: Instant::now(),
        });
    }

    fn on_stop(&self, _machine: &Machine, _state: &ActiveState<C>) {
        let mut guard = self.current_state.write().unwrap_or_else(PoisonError::into_inner);
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_detects_breached_sla() {
        let plugin = WatchdogPlugin::<serde_json::Value>::new(None)
            .with_state_timeout("processing", Duration::from_millis(50));

        let start = Instant::now();
        {
            let mut guard = plugin.current_state.write().unwrap();
            *guard = Some(ActiveStateRecord {
                machine_id: SmolStr::new("order_flow"),
                active_states: vec![SmolStr::new("processing")],
                entered_at: start,
            });
        }

        // Check before threshold
        assert_eq!(plugin.check(start + Duration::from_millis(20)), None);

        // Check after threshold
        let alert = plugin.check(start + Duration::from_millis(60)).expect("should trigger alert");
        assert_eq!(alert.machine_id.as_str(), "order_flow");
        assert_eq!(alert.active_states, vec![SmolStr::new("processing")]);
        assert_eq!(plugin.alerts_triggered(), 1);
    }
}
