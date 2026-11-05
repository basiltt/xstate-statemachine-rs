//! Sinks: where inspector messages go.
//!
//! [`Sink::send`] receives one protocol message at a time. [`MemorySink`] collects them in
//! memory (tests, the `xsm` CLI); [`JsonLinesSink`] appends them to a `.jsonl` file in the
//! Python `JsonLinesSink` format (one compact, ASCII-only JSON object per line, file created
//! owner-read/write only); [`read_jsonl`] reads such a recording back. A closure
//! `Fn(&Value) -> Result<(), SinkError>` is a sink too.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;
use xstate_core::pyjson::dumps_compact;

/// Failure to deliver a message. Through an [`crate::InspectorObserver`] the first failure
/// disables the observer (reported once); the machine is never affected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// The sink was closed before the send.
    #[error("sink is closed")]
    Closed,
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The message could not be encoded or the transport rejected it.
    #[error("{0}")]
    Other(String),
}

/// A destination for protocol messages. Implementations must be safe to call from several
/// threads; messages are JSON objects as produced by [`crate::protocol`].
pub trait Sink: Send + Sync {
    /// Deliver one message.
    ///
    /// # Errors
    /// Anything that stops delivery; see [`SinkError`].
    fn send(&self, message: &Value) -> Result<(), SinkError>;
}

impl<F> Sink for F
where
    F: Fn(&Value) -> Result<(), SinkError> + Send + Sync,
{
    fn send(&self, message: &Value) -> Result<(), SinkError> {
        self(message)
    }
}

/// Encode one message as a JSON line, exactly as Python's
/// `json.dumps(message, separators=(",", ":"), ensure_ascii=True)` does (without the newline).
#[must_use]
pub fn encode_line(message: &Value) -> String {
    dumps_compact(message)
}

/// Collects messages in memory, optionally keeping only the newest `max_len`.
#[derive(Debug, Default)]
pub struct MemorySink {
    inner: Mutex<MemoryInner>,
    max_len: Option<usize>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    messages: VecDeque<Value>,
    dropped: u64,
}

impl MemorySink {
    /// Keep every message.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep only the newest `max_len` messages (`max_len >= 1`); older ones are dropped and
    /// counted by [`MemorySink::dropped`].
    ///
    /// # Panics
    /// When `max_len` is 0.
    #[must_use]
    pub fn bounded(max_len: usize) -> Self {
        assert!(max_len >= 1, "MemorySink::bounded needs max_len >= 1");
        Self { inner: Mutex::default(), max_len: Some(max_len) }
    }

    /// Snapshot of the collected messages, oldest first.
    #[must_use]
    pub fn messages(&self) -> Vec<Value> {
        self.lock().messages.iter().cloned().collect()
    }

    /// Number of collected messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().messages.len()
    }

    /// `true` when nothing has been collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Messages that fell off a bounded sink.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Remove every collected message (the dropped counter is kept).
    pub fn clear(&self) {
        self.lock().messages.clear();
    }

    /// Take every collected message out, oldest first.
    pub fn drain(&self) -> Vec<Value> {
        self.lock().messages.drain(..).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Sink for MemorySink {
    fn send(&self, message: &Value) -> Result<(), SinkError> {
        let mut inner = self.lock();
        inner.messages.push_back(message.clone());
        if let Some(max) = self.max_len {
            while inner.messages.len() > max {
                inner.messages.pop_front();
                inner.dropped += 1;
            }
        }
        Ok(())
    }
}

/// Appends each message as one JSON line to a file, flushing after every write.
///
/// The file is created with mode `0o600` on Unix (it holds every event the machine saw); an
/// existing file is appended to and its mode left alone. Lines are pure ASCII. Sending after
/// [`JsonLinesSink::close`] fails with [`SinkError::Closed`].
#[derive(Debug)]
pub struct JsonLinesSink {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl JsonLinesSink {
    /// Open (or create) the recording at `path`.
    ///
    /// # Errors
    /// Filesystem failures.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SinkError> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self { path, file: Mutex::new(Some(file)) })
    }

    /// Recording path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the file; later sends fail. Idempotent.
    pub fn close(&self) {
        self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    }
}

impl Sink for JsonLinesSink {
    fn send(&self, message: &Value) -> Result<(), SinkError> {
        let mut line = encode_line(message);
        line.push('\n');
        let mut guard = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = guard.as_mut().ok_or(SinkError::Closed)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }
}

/// Read the messages of a [`JsonLinesSink`] recording.
///
/// Blank lines and lines that are not JSON objects are skipped. A truncated *last* line
/// (no trailing newline — a process that died mid-write) ends the iteration silently; a
/// corrupt line elsewhere is an error.
///
/// # Errors
/// I/O failures and corrupt lines, as [`SinkError`].
pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<Value>, SinkError> {
    let reader = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut reader = reader;
    loop {
        buf.clear();
        let read = reader.read_line(&mut buf)?;
        if read == 0 {
            break;
        }
        let terminated = buf.ends_with('\n');
        let stripped = buf.trim();
        if stripped.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(stripped) {
            Ok(Value::Object(obj)) => out.push(Value::Object(obj)),
            Ok(_) => {}
            Err(err) if terminated => {
                return Err(SinkError::Other(format!("corrupt line in recording: {err}")));
            }
            Err(_) => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_sink_bounds_and_counts_drops() {
        let sink = MemorySink::bounded(2);
        for i in 0..5 {
            sink.send(&json!({"i": i})).unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(sink.len(), 2);
        assert_eq!(sink.dropped(), 3);
        assert_eq!(sink.messages(), vec![json!({"i": 3}), json!({"i": 4})]);
        sink.clear();
        assert!(sink.is_empty());
    }

    #[test]
    fn closure_is_a_sink() {
        let seen = Mutex::new(0usize);
        let sink = |_: &Value| {
            *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            Ok(())
        };
        Sink::send(&sink, &json!({})).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(*seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 1);
    }

    #[test]
    fn jsonl_round_trip_and_truncated_tail() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("rec.jsonl");
        let sink = JsonLinesSink::open(&path).unwrap_or_else(|e| panic!("{e}"));
        sink.send(&json!({"type": "a", "s": "é"})).unwrap_or_else(|e| panic!("{e}"));
        sink.send(&json!({"type": "b"})).unwrap_or_else(|e| panic!("{e}"));
        sink.close();
        assert!(matches!(sink.send(&json!({})), Err(SinkError::Closed)));

        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(text, "{\"type\":\"a\",\"s\":\"\\u00e9\"}\n{\"type\":\"b\"}\n");
        // a truncated last line is tolerated; a corrupt middle line is not
        std::fs::write(&path, format!("{text}\n7\n{{\"type\":\"c\""))
            .unwrap_or_else(|e| panic!("{e}"));
        let msgs = read_jsonl(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(msgs.len(), 2);
        std::fs::write(&path, "{\"a\":1}\n{bad}\n{\"b\":2}\n").unwrap_or_else(|e| panic!("{e}"));
        assert!(read_jsonl(&path).is_err());
    }

    #[test]
    fn an_absurdly_deep_line_is_an_error_not_a_stack_overflow() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("deep.jsonl");
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let lines = format!("{{\"a\":1}}\n{deep}\n{{\"b\":2}}\n");
        std::fs::write(&path, lines).unwrap_or_else(|e| panic!("{e}"));
        let err = read_jsonl(&path).err().unwrap_or_else(|| panic!("a deep line must not parse"));
        assert!(err.to_string().contains("corrupt line"), "{err}");
    }
}
