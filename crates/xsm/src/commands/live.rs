//! `xsm inspect --live`: serve the Stately Inspector and drive a simulation from stdin.
//!
//! The machine runs in a [`Session`] (stub logic on a simulated clock, as in `xsm simulate`)
//! with an [`InspectorObserver`] attached, so the page served by the [`SseSink`] follows every
//! event live. Lines read from stdin drive it: `EVENT`, `EVENT {"json": "payload"}`, `+500`
//! (advance the simulated clock 500 ms), `reset` and `quit`. End of input also ends the run.
//!
//! Security follows the sink: loopback by default, a non-loopback `--host` needs `--token`,
//! and context is shown only for the keys named by `--context` (deny by default).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use xstate_core::{DynContext, Observer};
use xstate_inspect::{InspectorObserver, SseError, SseOptions, SseSink};

use super::analysis::{LogicNames, load_machine};
use super::model::short_id;
use super::session::{Command, Session, Step, StepKind, parse_events};
use crate::output::{self, EXIT_OK};
use crate::ui::{Role, Ui, sanitize};

/// How long a blocked wait sleeps between checks.
const SLICE: Duration = Duration::from_millis(250);

/// The reserved stdin words (everything else is an event name).
const WORD_QUIT: &str = "quit";
const WORD_RESET: &str = "reset";

/// Server flags shared by `inspect --live` and `replay --live`.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeArgs {
    /// Bind address. A non-loopback host requires --token.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port (0 picks a free one).
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Access token (default: a fresh random one per run). It is accepted in the URL once;
    /// the page swaps it for an `HttpOnly` cookie. API clients send `X-XSM-Token` or
    /// Authorization: Bearer.
    #[arg(long)]
    token: Option<String>,
    /// Open the page in a browser.
    #[arg(long)]
    open: bool,
}

/// The `--live` options of `inspect`.
#[derive(Debug, clap::Args)]
pub(crate) struct LiveOptions {
    #[command(flatten)]
    serve: ServeArgs,
    /// With --live: comma-separated context keys the page may see (default: none).
    #[arg(long, value_name = "KEYS")]
    context: Option<String>,
    /// With --live: run these events (`A,B,+500`) instead of reading stdin.
    #[arg(short, long, value_name = "EVENTS")]
    events: Option<String>,
    /// With --live: seconds to keep serving after the events or stdin end (default: until
    /// Ctrl-C with --events, exit at once after stdin).
    #[arg(long, value_name = "SECONDS")]
    duration: Option<f64>,
}

/// A finite, non-negative `--duration`.
pub(crate) fn parse_duration(seconds: Option<f64>) -> Result<Option<Duration>> {
    let Some(seconds) = seconds else { return Ok(None) };
    Duration::try_from_secs_f64(seconds).map(Some).map_err(|_| {
        output::usage(format!("--duration must be a finite number >= 0, got {seconds}"))
    })
}

/// Start the server and print its URL.
pub(crate) fn serve(ui: Ui, args: &ServeArgs) -> Result<Arc<SseSink>> {
    let options = SseOptions {
        host: args.host.clone(),
        port: args.port,
        token: args.token.clone(),
        ..SseOptions::default()
    };
    let sink = Arc::new(SseSink::bind(options).map_err(serve_error)?);
    println!("live inspector: {}", sink.url());
    println!(
        "{}",
        ui.paint(
            Role::Muted,
            "the token travels in the URL only on first load: the page swaps it for an HttpOnly \
             cookie"
        )
    );
    if args.open && !open_browser(&sink.url()) {
        eprintln!("xsm: warning: could not open a browser; open the URL above");
    }
    Ok(sink)
}

fn serve_error(err: SseError) -> anyhow::Error {
    match err {
        SseError::TokenRequired(_) | SseError::InvalidToken => {
            output::usage(format!("{err} -- pass --token"))
        }
        other => output::failure(other.to_string()),
    }
}

/// Ask the desktop to open `url`; `false` when there is no way to (a headless box).
fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(windows) {
        let mut c = Process::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        Process::new("open")
    } else {
        Process::new("xdg-open")
    };
    command.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    command.status().is_ok_and(|status| status.success())
}

/// Sleep for `duration` (forever when `None`); short slices so Ctrl-C lands promptly.
pub(crate) fn block(duration: Option<Duration>) {
    let deadline = duration.map(|d| Instant::now() + d);
    loop {
        let nap = match deadline {
            None => SLICE,
            Some(end) => {
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return;
                }
                left.min(SLICE)
            }
        };
        std::thread::sleep(nap);
    }
}

/// Run `inspect --live`.
pub(crate) fn run_inspect_live(ui: Ui, file: &Path, options: &LiveOptions) -> Result<u8> {
    let machine = load_machine(ui, file)?;
    let known: Vec<String> = LogicNames::collect(&machine).guards.into_iter().collect();
    let duration = parse_duration(options.duration)?;
    let script = options.events.as_deref().map(parse_events).transpose().map_err(output::usage)?;
    let sink = serve(ui, &options.serve)?;
    let keys = context_keys(options.context.as_deref());
    let observer: Arc<dyn Observer<DynContext>> =
        Arc::new(InspectorObserver::with_shared_sink(sink.clone()).context_allowlist(keys));
    let outcome = Session::new(machine, true, BTreeMap::new(), known, Some(observer))
        .and_then(|mut session| drive(ui, &mut session, script.as_deref(), duration));
    sink.close();
    outcome.map(|()| EXIT_OK)
}

fn context_keys(text: Option<&str>) -> Vec<String> {
    let keys = text.unwrap_or_default().split(',');
    keys.map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect()
}

/// Feed the session from `script` (then stay up for `duration`) or from stdin.
fn drive(
    ui: Ui,
    session: &mut Session,
    script: Option<&[Command]>,
    duration: Option<Duration>,
) -> Result<()> {
    print_steps(ui, session, 0);
    if let Some(commands) = script {
        run_commands(ui, session, commands)?;
        block(duration);
        return Ok(());
    }
    println!(
        "{}",
        ui.paint(Role::Muted, "type EVENT, EVENT {\"json\": \"payload\"}, +MS, reset or quit")
    );
    repl(ui, session, std::io::stdin().lock())?;
    if duration.is_some() {
        block(duration);
    }
    Ok(())
}

/// What one line of input asks for.
#[derive(Debug, PartialEq)]
enum Input {
    Skip,
    Quit,
    Run(Vec<Command>),
}

/// Read lines until `quit` or end of input; a bad line is reported and skipped.
fn repl(ui: Ui, session: &mut Session, input: impl BufRead) -> Result<()> {
    for line in input.lines() {
        match parse_line(&line?) {
            Ok(Input::Quit) => break,
            Ok(Input::Skip) => {}
            Ok(Input::Run(commands)) => run_commands(ui, session, &commands)?,
            Err(message) => eprintln!("xsm: {}", sanitize(&message)),
        }
    }
    Ok(())
}

fn parse_line(line: &str) -> Result<Input, String> {
    let line = line.trim();
    match line {
        "" => return Ok(Input::Skip),
        WORD_QUIT => return Ok(Input::Quit),
        WORD_RESET => return Ok(Input::Run(vec![Command::Reset])),
        _ if line.starts_with('+') => return parse_events(line).map(Input::Run),
        _ => {}
    }
    let (event, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let payload = parse_payload(event, rest.trim())?;
    Ok(Input::Run(vec![Command::Send { event: event.to_owned(), payload }]))
}

/// The JSON object after an event name (`Null` when there is none).
fn parse_payload(event: &str, text: &str) -> Result<Value, String> {
    if text.is_empty() {
        return Ok(Value::Null);
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("{event}: the payload is not valid JSON: {e}"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(format!("{event}: the payload must be a JSON object"))
    }
}

fn run_commands(ui: Ui, session: &mut Session, commands: &[Command]) -> Result<()> {
    let from = session.history().len();
    for command in commands {
        session.apply(command)?;
    }
    print_steps(ui, session, from);
    Ok(())
}

/// Print the history rows from `from` on.
fn print_steps(ui: Ui, session: &Session, from: usize) {
    for step in &session.history()[from..] {
        println!("{}", step_line(ui, session, step));
    }
}

fn step_line(ui: Ui, session: &Session, step: &Step) -> String {
    let outcome = if let Some(error) = &step.error {
        ui.paint(Role::Err, &format!("error: {}", sanitize(error)))
    } else if step.changed() || matches!(step.kind, StepKind::Start | StepKind::Reset) {
        ui.paint(Role::Ok, &states_text(session, &step.after))
    } else if step.unhandled {
        ui.paint(Role::Warn, "(unhandled)")
    } else {
        ui.paint(Role::Muted, "(no change)")
    };
    format!("  {:<16} {outcome}", sanitize(&step.label))
}

fn states_text(session: &Session, ids: &[String]) -> String {
    let machine = session.machine();
    ids.iter().map(|id| short_id(machine, id)).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lines_map_to_commands() {
        assert_eq!(parse_line("  "), Ok(Input::Skip));
        assert_eq!(parse_line("quit"), Ok(Input::Quit));
        assert_eq!(parse_line("reset"), Ok(Input::Run(vec![Command::Reset])));
        assert_eq!(parse_line("+500"), Ok(Input::Run(vec![Command::Clock(500.0)])));
        assert_eq!(
            parse_line("TOGGLE"),
            Ok(Input::Run(vec![Command::Send { event: "TOGGLE".into(), payload: Value::Null }]))
        );
        assert_eq!(
            parse_line("PAY  {\"n\": 1}"),
            Ok(Input::Run(vec![Command::Send { event: "PAY".into(), payload: json!({"n": 1}) }]))
        );
    }

    #[test]
    fn bad_lines_are_reported() {
        assert!(parse_line("+abc").is_err());
        assert!(parse_line("PAY {oops").is_err());
        assert!(parse_line("PAY [1]").is_err());
    }

    #[test]
    fn durations_must_be_finite_and_non_negative() {
        assert!(matches!(parse_duration(None), Ok(None)));
        assert!(
            matches!(parse_duration(Some(1.5)), Ok(Some(d)) if d == Duration::from_millis(1500))
        );
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(parse_duration(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn context_keys_are_trimmed_and_blanks_dropped() {
        assert_eq!(context_keys(Some(" a, b ,,c")), ["a", "b", "c"]);
        assert_eq!(context_keys(None).len(), 0);
    }
}
