//! `xsm replay`: print a recorded inspector session, or stream it (`--live`) to the page.
//!
//! The recording is a JSON Lines file of Stately Inspector messages, as written by
//! `xsm simulate --record` or the Python library. Spelling and options follow the Python
//! `xsm replay`: without `--live` the messages are printed one per row; with `--live` the page
//! is served and the messages are replayed through it at `--speed` (`0` at once, `1.0` at the
//! recorded pace), after which the server stays up for `--duration` seconds (default: until
//! Ctrl-C).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use xstate_inspect::protocol::is_message_type;
use xstate_inspect::{ReplayOptions, read_jsonl, replay_file};

use super::live::{ServeArgs, block, parse_duration, serve};
use crate::output::{self, EXIT_OK};
use crate::ui::{Ui, sanitize};

/// Arguments of `replay`.
#[derive(Debug, clap::Args)]
pub(crate) struct ReplayArgs {
    /// The recording (.jsonl).
    file: PathBuf,
    /// Serve it on the live inspector instead of printing.
    #[arg(long)]
    live: bool,
    /// With --live: 1.0 replays at recorded pace, 2.0 twice as fast, 0 (default) at once.
    /// Negative is refused.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    speed: f64,
    /// With --live: seconds to keep serving (default: until Ctrl-C).
    #[arg(long, value_name = "SECONDS")]
    duration: Option<f64>,
    #[command(flatten)]
    serve: ServeArgs,
}

/// Run `replay`.
pub(crate) fn run(ui: Ui, args: &ReplayArgs) -> Result<u8> {
    if args.speed.is_nan() || args.speed.is_infinite() || args.speed < 0.0 {
        return Err(output::usage(format!("--speed must be >= 0, got {}", args.speed)));
    }
    let duration = parse_duration(args.duration)?;
    if !args.file.is_file() {
        return Err(output::failure(format!("{}: no such file", args.file.display())));
    }
    if !args.live {
        print_rows(&args.file)?;
        return Ok(EXIT_OK);
    }
    let sink = serve(ui, &args.serve)?;
    let replayed = replay_file(&args.file, &*sink, ReplayOptions { speed: args.speed })
        .map_err(|e| output::failure(format!("{}: {e}", args.file.display())))
        .and_then(|sent| refuse_if_empty(&args.file, sent, 0).map(|()| sent));
    match replayed {
        Ok(sent) => {
            println!("replayed {sent} messages");
            block(duration);
            sink.close();
            Ok(EXIT_OK)
        }
        Err(err) => {
            sink.close();
            Err(err)
        }
    }
}

/// Print one row per inspector message: `type  session  event  value`.
fn print_rows(path: &Path) -> Result<()> {
    let messages =
        read_jsonl(path).map_err(|e| output::failure(format!("{}: {e}", path.display())))?;
    let mut shown = 0;
    let mut skipped = 0;
    for message in &messages {
        if !message.get("type").and_then(Value::as_str).is_some_and(is_message_type) {
            skipped += 1;
            continue;
        }
        shown += 1;
        println!("{}", row(message));
    }
    refuse_if_empty(path, shown, skipped)
}

fn row(message: &Value) -> String {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or("").to_owned();
    let kind = text(message.get("type"));
    let session = text(message.get("sessionId"));
    let event = text(message.get("event").and_then(|e| e.get("type")));
    let value = message.get("snapshot").and_then(|s| s.get("value")).unwrap_or(&Value::Null);
    sanitize(&format!("{kind:<17} {session}  {event}  {value}"))
}

/// A non-empty file with no inspector message in it is not a recording.
///
/// For a live replay `skipped` is unknown, so a replay that sent nothing from a file that
/// holds any bytes counts as "not a recording".
fn refuse_if_empty(path: &Path, shown: usize, skipped: usize) -> Result<()> {
    let holds_data = skipped > 0 || std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
    if shown > 0 || !holds_data {
        return Ok(());
    }
    Err(output::failure(format!(
        "{}: no inspector messages (`@xstate.*`) -- not a recording; record one with \
         `xsm simulate --record`",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_show_type_session_event_and_value() {
        let message = json!({
            "type": "@xstate.snapshot", "sessionId": "toggle",
            "event": {"type": "TOGGLE"}, "snapshot": {"value": "active"}
        });
        assert_eq!(row(&message), "@xstate.snapshot  toggle  TOGGLE  \"active\"");
        let bare = row(&json!({"type": "@xstate.actor"}));
        assert!(bare.starts_with("@xstate.actor") && bare.ends_with("  null"), "{bare:?}");
    }

    #[test]
    fn control_characters_in_a_recording_cannot_forge_output() {
        let message =
            json!({"type": "@xstate.event", "sessionId": "a\nb", "event": {"type": "\u{1b}[2J"}});
        assert!(!row(&message).contains(['\n', '\u{1b}']));
    }
}
