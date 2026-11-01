//! The simulation session behind `xsm simulate`: a machine on a simulated clock with stub
//! logic, a script of commands to drive it, and the history it leaves behind.
//!
//! Guards are constants (`true` unless told otherwise). Changing a guard rebuilds the logic and
//! restores the running interpreter from its own snapshot, so the configuration, context and
//! pending timers survive the swap.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use xstate_core::graph::{GuardPolicy, stub_logic};
use xstate_core::{
    Clock, DynContext, Event, Machine, MachineLogic, Observer, RestartTimers, RestoreOptions,
    SimulatedClock, Status, SyncInterpreter,
};

use crate::output;

/// One thing the user asked the session to do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// Deliver an event, with an optional object payload.
    Send { event: String, payload: Value },
    /// Advance the simulated clock by this many milliseconds.
    Clock(f64),
    /// Make a guard return a constant from now on.
    Guard { name: String, value: bool },
    /// Restart the machine from its initial state on a fresh clock.
    Reset,
}

/// A finite, non-negative millisecond count.
fn check_ms(ms: f64, shown: &str) -> Result<f64, String> {
    if ms.is_finite() && ms >= 0.0 {
        return Ok(ms);
    }
    Err(format!(
        "bad clock advance {shown:?}: expected +N with N a finite, non-negative number of \
         milliseconds"
    ))
}

fn parse_ms(text: &str, shown: &str) -> Result<f64, String> {
    check_ms(text.trim().parse::<f64>().unwrap_or(f64::NAN), shown)
}

/// Parse `A,B,+500,C`: names are events, `+N` advances the clock by N milliseconds.
pub(crate) fn parse_events(text: &str) -> Result<Vec<Command>, String> {
    let mut commands = Vec::new();
    for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(ms) = token.strip_prefix('+') {
            commands.push(Command::Clock(parse_ms(ms, token)?));
        } else {
            commands.push(Command::Send { event: token.to_owned(), payload: Value::Null });
        }
    }
    Ok(commands)
}

/// Validate one script entry.
fn parse_command(entry: &Value, where_: &str) -> Result<Command, String> {
    let Some(object) = entry.as_object() else {
        return Err(format!("{where_}: expected an object, got {entry}"));
    };
    if let Some(event) = object.get("send") {
        let Some(event) = event.as_str().filter(|e| !e.is_empty()) else {
            return Err(format!("{where_}: 'send' must be a non-empty event name"));
        };
        let payload = match object.get("payload") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(_) => return Err(format!("{where_}: 'payload' must be an object")),
        };
        return Ok(Command::Send { event: event.to_owned(), payload });
    }
    if let Some(ms) = object.get("clock") {
        let Some(ms) = ms.as_f64() else {
            return Err(format!("{where_}: 'clock' must be a number of milliseconds"));
        };
        return check_ms(ms, &format!("clock {ms}"))
            .map(Command::Clock)
            .map_err(|e| format!("{where_}: {e}"));
    }
    if let Some(name) = object.get("guard") {
        let Some(name) = name.as_str() else {
            return Err(format!("{where_}: 'guard' must be a guard name"));
        };
        let value = match object.get("value") {
            None => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(format!("{where_}: 'value' must be true or false")),
        };
        return Ok(Command::Guard { name: name.to_owned(), value });
    }
    if object.get("reset") == Some(&Value::Bool(true)) {
        return Ok(Command::Reset);
    }
    Err(format!("{where_}: unknown command {entry} (expected send / clock / guard / reset)"))
}

/// Read a `--script` file: a JSON list of commands.
pub(crate) fn load_script(path: &Path) -> Result<Vec<Command>, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("--script {shown}: {e}"))?;
    if meta.len() > xstate_core::DEFAULT_MAX_INPUT_BYTES as u64 {
        return Err(format!(
            "--script {shown}: larger than {} bytes",
            xstate_core::DEFAULT_MAX_INPUT_BYTES
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("--script {shown}: {e}"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("--script {shown}: not valid JSON: {e}"))?;
    let Some(entries) = value.as_array() else {
        return Err(format!("--script {shown}: expected a JSON list of commands"));
    };
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| parse_command(entry, &format!("--script {shown}: step {}", i + 1)))
        .collect()
}

/// What kind of step a history row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// Entering the initial configuration.
    Start,
    /// An event delivered.
    Event,
    /// A clock advance.
    Clock,
    /// A guard constant changed.
    Guard,
    /// A restart.
    Reset,
}

impl StepKind {
    /// Lower-case name used in `--json`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Event => "event",
            Self::Clock => "clock",
            Self::Guard => "guard",
            Self::Reset => "reset",
        }
    }
}

/// One history row.
#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub(crate) kind: StepKind,
    pub(crate) label: String,
    pub(crate) before: Vec<String>,
    pub(crate) after: Vec<String>,
    pub(crate) unhandled: bool,
    pub(crate) actions: Vec<String>,
    pub(crate) error: Option<String>,
    pub(crate) clock_ms: f64,
}

impl Step {
    /// Whether the active configuration changed.
    pub(crate) fn changed(&self) -> bool {
        self.before != self.after
    }

    /// The Python-compatible history record.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "label": self.label,
            "before": self.before,
            "after": self.after,
            "changed": self.changed(),
            "unhandled": self.unhandled,
            "actions": self.actions,
            "error": self.error,
            "clock_ms": self.clock_ms,
        })
    }
}

/// A machine, a virtual clock and a history, driven by [`Command`]s.
pub(crate) struct Session {
    machine: Machine,
    clock: SimulatedClock,
    interp: SyncInterpreter<DynContext>,
    default_guard: bool,
    guards: BTreeMap<String, bool>,
    known_guards: Vec<String>,
    history: Vec<Step>,
    /// Re-attached to every interpreter the session builds (guard swaps, resets).
    observer: Option<Arc<dyn Observer<DynContext>>>,
}

/// Stub logic with every guard constant: `default_guard`, except the named overrides.
fn build_logic(
    machine: &Machine,
    default_guard: bool,
    overrides: &BTreeMap<String, bool>,
) -> MachineLogic<DynContext> {
    let policy = if default_guard { GuardPolicy::True } else { GuardPolicy::False };
    let mut logic = stub_logic::<DynContext>(machine, policy);
    for (name, value) in overrides {
        let value = *value;
        logic = logic.guard(name.clone(), move |_, _, _| Ok(value));
    }
    logic
}

impl Session {
    /// Start the machine on a fresh clock. `observer` (an inspector recorder, say) sees every
    /// interpreter the session builds.
    pub(crate) fn new(
        machine: Machine,
        default_guard: bool,
        guards: BTreeMap<String, bool>,
        known_guards: Vec<String>,
        observer: Option<Arc<dyn Observer<DynContext>>>,
    ) -> Result<Self> {
        let clock = SimulatedClock::new();
        let logic = build_logic(&machine, default_guard, &guards);
        let interp = SyncInterpreter::with_clock(machine.clone(), logic, Arc::new(clock.clone()));
        let mut session = Self {
            machine,
            clock,
            interp,
            default_guard,
            guards,
            known_guards,
            history: Vec::new(),
            observer,
        };
        session.attach_observer();
        session.start_interpreter(StepKind::Start, "(start)", Vec::new())?;
        Ok(session)
    }

    fn attach_observer(&mut self) {
        if let Some(observer) = &self.observer {
            self.interp.observe(observer.clone());
        }
    }

    /// Enter the initial configuration and record it as a `Start` or `Reset` row.
    fn start_interpreter(
        &mut self,
        kind: StepKind,
        label: &str,
        before: Vec<String>,
    ) -> Result<()> {
        let receipt = self
            .interp
            .start()
            .map_err(|e| output::failure(format!("cannot start '{}': {e}", self.machine.id())))?;
        self.history.push(Step {
            kind,
            label: label.to_owned(),
            before,
            after: receipt.state_ids(),
            unhandled: false,
            actions: receipt.action_names(),
            error: None,
            clock_ms: self.clock.now_ms(),
        });
        Ok(())
    }

    /// Apply one command and record it.
    pub(crate) fn apply(&mut self, command: &Command) -> Result<()> {
        match command {
            Command::Send { event, payload } => self.send(event, payload),
            Command::Clock(ms) => self.advance(*ms),
            Command::Guard { name, value } => self.set_guard(name, *value)?,
            Command::Reset => self.reset()?,
        }
        Ok(())
    }

    fn record(&mut self, kind: StepKind, label: String, before: Vec<String>, outcome: Outcome) {
        self.history.push(Step {
            kind,
            label,
            before,
            after: self.interp.state_ids(),
            unhandled: outcome.unhandled,
            actions: outcome.actions,
            error: outcome.error,
            clock_ms: self.clock.now_ms(),
        });
    }

    fn send(&mut self, event: &str, payload: &Value) {
        let before = self.interp.state_ids();
        let event_value = Event::new(event).payload(payload.clone());
        let outcome = Outcome::from(self.interp.send(event_value));
        self.record(StepKind::Event, event.to_owned(), before, outcome);
    }

    fn advance(&mut self, ms: f64) {
        let before = self.interp.state_ids();
        let outcome = Outcome::from(self.interp.advance(&self.clock, ms));
        self.record(StepKind::Clock, format!("+{ms} ms"), before, outcome);
    }

    fn set_guard(&mut self, name: &str, value: bool) -> Result<()> {
        if !self.known_guards.iter().any(|g| g == name) {
            return Err(output::usage(unknown_guard_message(name, &self.known_guards)));
        }
        let before = self.interp.state_ids();
        self.guards.insert(name.to_owned(), value);
        let snapshot = self.interp.snapshot()?;
        let logic = build_logic(&self.machine, self.default_guard, &self.guards);
        let options = RestoreOptions { verify_hash: None, restart_timers: RestartTimers::Resume };
        self.interp = SyncInterpreter::from_snapshot(
            self.machine.clone(),
            logic,
            &snapshot,
            Arc::new(self.clock.clone()),
            &options,
        )?;
        self.attach_observer();
        self.record(StepKind::Guard, format!("guard {name}={value}"), before, Outcome::default());
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.interp.stop();
        self.clock = SimulatedClock::new();
        let logic = build_logic(&self.machine, self.default_guard, &self.guards);
        self.interp =
            SyncInterpreter::with_clock(self.machine.clone(), logic, Arc::new(self.clock.clone()));
        self.attach_observer();
        let before = self.history.last().map(|s| s.after.clone()).unwrap_or_default();
        self.start_interpreter(StepKind::Reset, "reset", before)
    }

    /// The machine being simulated.
    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Everything that happened, the start row first.
    pub(crate) fn history(&self) -> &[Step] {
        &self.history
    }

    /// The interpreter (read-only).
    pub(crate) fn interpreter(&self) -> &SyncInterpreter<DynContext> {
        &self.interp
    }

    /// Simulated time elapsed, in milliseconds.
    pub(crate) fn clock_ms(&self) -> f64 {
        self.clock.now_ms()
    }

    /// Constant guards set so far (explicit overrides only).
    pub(crate) fn guard_overrides(&self) -> &BTreeMap<String, bool> {
        &self.guards
    }

    /// Value of every guard the machine does not override explicitly.
    pub(crate) fn default_guard(&self) -> bool {
        self.default_guard
    }

    /// Whether any step reported an error.
    pub(crate) fn had_errors(&self) -> bool {
        self.history.iter().any(|s| s.error.is_some())
    }

    /// Final status of the machine.
    pub(crate) fn status(&self) -> Status {
        self.interp.status()
    }
}

/// `unknown guard 'x'; the machine uses: a, b` (or says it has none).
pub(crate) fn unknown_guard_message(name: &str, known: &[String]) -> String {
    if known.is_empty() {
        format!("unknown guard '{name}': this machine has no named guards")
    } else {
        format!("unknown guard '{name}'; the machine uses: {}", known.join(", "))
    }
}

/// What delivering an event or advancing the clock produced.
#[derive(Debug, Default)]
struct Outcome {
    unhandled: bool,
    actions: Vec<String>,
    error: Option<String>,
}

impl From<xstate_core::Result<xstate_core::Receipt>> for Outcome {
    fn from(result: xstate_core::Result<xstate_core::Receipt>) -> Self {
        match result {
            Ok(receipt) => {
                Self { unhandled: receipt.unhandled, actions: receipt.action_names(), error: None }
            }
            Err(err) => {
                Self { unhandled: false, actions: Vec::new(), error: Some(err.to_string()) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_argument_mixes_events_and_clock_advances() {
        let commands = parse_events("A, +500 ,B").unwrap();
        assert_eq!(
            commands,
            vec![
                Command::Send { event: "A".into(), payload: Value::Null },
                Command::Clock(500.0),
                Command::Send { event: "B".into(), payload: Value::Null },
            ]
        );
    }

    #[test]
    fn clock_advance_rejects_garbage() {
        assert!(parse_events("+abc").is_err());
        assert!(parse_events("+-5").is_err());
        assert!(parse_events("+inf").is_err());
        assert!(parse_events("+NaN").is_err());
    }

    #[test]
    fn script_commands_are_validated() {
        let ok = parse_command(&json!({"send": "GO", "payload": {"n": 1}}), "t").unwrap();
        assert_eq!(ok, Command::Send { event: "GO".into(), payload: json!({"n": 1}) });
        assert!(parse_command(&json!({"send": ""}), "t").is_err());
        assert!(parse_command(&json!({"undo": true}), "t").is_err());
        assert!(parse_command(&json!({"guard": "g", "value": "no"}), "t").is_err());
        assert_eq!(parse_command(&json!({"reset": true}), "t").unwrap(), Command::Reset);
    }
}
