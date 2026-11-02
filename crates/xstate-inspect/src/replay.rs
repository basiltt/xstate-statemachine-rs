//! Replay a recorded inspector session into a sink.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::protocol::is_message_type;
use crate::sinks::{Sink, SinkError, read_jsonl};

/// The longest pause [`replay_with`] asks `sleep` for between two messages (one hour), so a
/// recording with a huge or corrupt `createdAt` gap, or a tiny `speed`, cannot hang a replay.
pub const MAX_REPLAY_GAP: Duration = Duration::from_secs(3600);

/// Replay options.
#[derive(Debug, Clone, Copy)]
pub struct ReplayOptions {
    /// `0.0` (default) sends as fast as possible; `1.0` honours the recorded `createdAt`
    /// gaps; `2.0` is twice as fast. Must be finite and `>= 0`. A single gap is capped at
    /// [`MAX_REPLAY_GAP`] after scaling.
    pub speed: f64,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self { speed: 0.0 }
    }
}

/// Errors raised by [`replay`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReplayError {
    /// `speed` is negative or NaN.
    #[error("speed must be >= 0, got {0}")]
    InvalidSpeed(f64),
    /// Reading the recording or sending failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// Send every protocol message of `messages` to `sink`, pacing with `sleep` when
/// `options.speed > 0`. Returns the number of messages sent; entries whose `type` is not a
/// protocol message type are skipped, never forwarded.
///
/// # Errors
/// [`ReplayError::InvalidSpeed`], or the first sink failure.
pub fn replay_with<'a>(
    messages: impl IntoIterator<Item = &'a Value>,
    sink: &dyn Sink,
    options: ReplayOptions,
    mut sleep: impl FnMut(Duration),
) -> Result<usize, ReplayError> {
    if options.speed.is_nan() || options.speed < 0.0 || !options.speed.is_finite() {
        return Err(ReplayError::InvalidSpeed(options.speed));
    }
    let mut sent = 0;
    let mut last: Option<i64> = None;
    for msg in messages {
        if !msg.get("type").and_then(Value::as_str).is_some_and(is_message_type) {
            continue;
        }
        if options.speed > 0.0 {
            let at = created_at(msg);
            if let (Some(at), Some(prev)) = (at, last)
                && let Some(gap_ms) = at.checked_sub(prev).filter(|gap| *gap > 0)
            {
                sleep(scaled_gap(gap_ms, options.speed));
            }
            if at.is_some() {
                last = at;
            }
        }
        sink.send(msg)?;
        sent += 1;
    }
    Ok(sent)
}

/// A recorded gap in milliseconds divided by `speed`, as a pause of at most [`MAX_REPLAY_GAP`]
/// (a scaled gap that is not representable counts as the cap).
fn scaled_gap(gap_ms: i64, speed: f64) -> Duration {
    #[allow(clippy::cast_precision_loss)]
    let seconds = gap_ms as f64 / 1000.0 / speed;
    Duration::try_from_secs_f64(seconds).map_or(MAX_REPLAY_GAP, |gap| gap.min(MAX_REPLAY_GAP))
}

/// [`replay_with`] sleeping on the current thread.
///
/// # Errors
/// See [`replay_with`].
pub fn replay<'a>(
    messages: impl IntoIterator<Item = &'a Value>,
    sink: &dyn Sink,
    options: ReplayOptions,
) -> Result<usize, ReplayError> {
    replay_with(messages, sink, options, std::thread::sleep)
}

/// Replay a `.jsonl` recording written by [`crate::JsonLinesSink`].
///
/// # Errors
/// Unreadable or corrupt recording, invalid speed, or a sink failure.
pub fn replay_file(
    path: impl AsRef<Path>,
    sink: &dyn Sink,
    options: ReplayOptions,
) -> Result<usize, ReplayError> {
    let messages = read_jsonl(path)?;
    replay(&messages, sink, options)
}

/// `createdAt` as an integer (the protocol carries it as a decimal string; numbers accepted).
fn created_at(msg: &Value) -> Option<i64> {
    match msg.get("createdAt") {
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::MemorySink;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn gaps_are_capped_and_never_panic() {
        let sink = MemorySink::new();
        let slept = RefCell::new(Vec::new());
        let messages = [
            msg("@xstate.actor", "0"),
            msg("@xstate.event", &i64::MAX.to_string()),
            msg("@xstate.snapshot", &i64::MIN.to_string()),
            msg("@xstate.snapshot", "5"),
        ];
        let fast = ReplayOptions { speed: 1e-300 };
        let sent = replay_with(&messages, &sink, fast, |d| slept.borrow_mut().push(d))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sent, 4);
        // `i64::MAX - 0` is capped; `MIN - MAX` overflows and is skipped; `5 - MIN` overflows.
        assert_eq!(*slept.borrow(), [MAX_REPLAY_GAP]);
    }

    fn msg(kind: &str, at: &str) -> Value {
        json!({"type": kind, "createdAt": at, "sessionId": "s"})
    }

    #[test]
    fn skips_non_protocol_messages_and_paces_by_created_at() {
        let messages = vec![
            msg("@xstate.actor", "1000"),
            json!({"type": "note"}),
            msg("@xstate.event", "1500"),
            msg("@xstate.snapshot", "bogus"),
            msg("@xstate.event", "1400"),
            msg("@xstate.event", "2400"),
        ];
        let sink = MemorySink::new();
        let naps = RefCell::new(Vec::new());
        let sent = replay_with(&messages, &sink, ReplayOptions { speed: 2.0 }, |d| {
            naps.borrow_mut().push(d);
        })
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sent, 5);
        assert_eq!(sink.len(), 5);
        assert_eq!(*naps.borrow(), vec![Duration::from_millis(250), Duration::from_millis(500)]);
    }

    #[test]
    fn speed_zero_never_sleeps_and_negative_is_rejected() {
        let messages = vec![msg("@xstate.event", "1"), msg("@xstate.event", "9000")];
        let sink = MemorySink::new();
        let slept = RefCell::new(false);
        replay_with(&messages, &sink, ReplayOptions::default(), |_| *slept.borrow_mut() = true)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!*slept.borrow());
        assert!(matches!(
            replay(&messages, &sink, ReplayOptions { speed: -1.0 }),
            Err(ReplayError::InvalidSpeed(_))
        ));
        assert!(replay(&messages, &sink, ReplayOptions { speed: f64::NAN }).is_err());
    }

    #[test]
    fn sink_failure_stops_the_replay() {
        let messages = vec![msg("@xstate.event", "1"), msg("@xstate.event", "2")];
        let failing = |_: &Value| Err(SinkError::Closed);
        assert!(matches!(
            replay(&messages, &failing, ReplayOptions::default()),
            Err(ReplayError::Sink(_))
        ));
    }
}
