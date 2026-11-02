//! End-to-end tests of `xsm inspect --live` and `xsm replay --live`: the binary runs with a
//! piped stdin/stdout, and the SSE server it prints the URL of is queried over a raw socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use common::{fixture, xsm};
use predicates::prelude::*;
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(20);

/// A running `xsm` child with its stdout streamed line by line; killed on drop.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xsm"))
            .args(args)
            .env_remove("NO_COLOR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self { child, stdin, lines, seen: Vec::new() }
    }

    /// Wait for a stdout line satisfying `pred` and return it.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(found) = self.seen.iter().find(|l| pred(l)) {
                return found.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {what}: {:?}", self.seen)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("stdout closed waiting for {what}: {:?}", self.seen)
                }
            }
        }
    }

    fn send_line(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{text}").unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// The server URL the process printed: `(port, token)`.
    fn endpoint(&mut self) -> Endpoint {
        let line = self.wait_for("the live inspector URL", |l| l.starts_with("live inspector: "));
        let url = line.strip_prefix("live inspector: ").unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap_or_else(|| panic!("{url}"));
        let (port, token) = rest.split_once("/?token=").unwrap();
        Endpoint { port: port.parse().unwrap(), token: token.to_owned() }
    }

    /// Wait for the process to exit on its own and return its exit code.
    fn exit_code(&mut self) -> i32 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap();
            }
            assert!(Instant::now() < deadline, "the process did not exit: {:?}", self.seen);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Endpoint {
    port: u16,
    token: String,
}

impl Endpoint {
    /// `GET path` with the token as a bearer credential; returns `(status, body)`.
    fn get(&self, path: &str) -> (u16, String) {
        self.get_with(path, &format!("Authorization: Bearer {}\r\n", self.token))
    }

    fn get_with(&self, path: &str, extra: &str) -> (u16, String) {
        self.get_as_host(path, &format!("127.0.0.1:{}", self.port), extra)
    }

    /// `GET path` presenting `host` in the `Host` header.
    fn get_as_host(&self, path: &str, host: &str, extra: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text);
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, body.to_owned())
    }

    fn messages(&self) -> Vec<Value> {
        let (status, body) = self.get("/messages");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
}

fn types_of(messages: &[Value]) -> Vec<String> {
    messages.iter().map(|m| m["type"].as_str().unwrap().to_owned()).collect()
}

fn event_names(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m["type"] == "@xstate.event")
        .map(|m| m["event"]["type"].as_str().unwrap().to_owned())
        .collect()
}

fn toggle() -> String {
    fixture("toggle.json").display().to_string()
}

// -------------------------------------------------------------------------- inspect --live

#[test]
fn live_serves_the_inspector_and_follows_stdin_events() {
    let mut run = Running::spawn(&["inspect", &toggle(), "--live", "--port", "0"]);
    let endpoint = run.endpoint();
    run.send_line("TOGGLE");
    run.wait_for("the TOGGLE step", |l| l.contains("TOGGLE") && l.contains("active"));

    let messages = endpoint.messages();
    assert_eq!(types_of(&messages)[0], "@xstate.actor");
    assert!(event_names(&messages).contains(&"TOGGLE".to_owned()), "{messages:?}");
    let last = messages.last().unwrap();
    assert_eq!(last["type"], "@xstate.snapshot");
    assert_eq!(last["snapshot"]["value"], "active");

    run.send_line("quit");
    assert_eq!(run.exit_code(), 0);
}

#[test]
fn live_is_protected_by_the_token_and_the_host_check() {
    let mut run = Running::spawn(&["inspect", &toggle(), "--live", "--port", "0"]);
    let endpoint = run.endpoint();
    assert_eq!(endpoint.get_with("/messages", "").0, 401);
    let first_load = endpoint.get_with(&format!("/?token={}", endpoint.token), "");
    assert_eq!(first_load.0, 303);
    let bad_host = endpoint.get_as_host(
        "/messages",
        "evil.example",
        &format!("X-XSM-Token: {}\r\n", endpoint.token),
    );
    assert_eq!(bad_host.0, 421);
    run.send_line("quit");
    assert_eq!(run.exit_code(), 0);
}

#[test]
fn live_exits_cleanly_at_end_of_input_and_reset_restarts_the_actor() {
    let mut run = Running::spawn(&["inspect", &toggle(), "--live", "--port", "0"]);
    let endpoint = run.endpoint();
    run.send_line("TOGGLE");
    run.send_line("reset");
    run.wait_for("the reset step", |l| l.trim_start().starts_with("reset "));
    let messages = endpoint.messages();
    let actors = types_of(&messages).iter().filter(|t| *t == "@xstate.actor").count();
    assert_eq!(actors, 2, "{messages:?}");
    run.close_stdin();
    assert_eq!(run.exit_code(), 0);
}

#[test]
fn live_clock_lines_and_bad_lines_do_not_stop_the_session() {
    let mut run = Running::spawn(&["inspect", &toggle(), "--live", "--port", "0"]);
    run.endpoint();
    run.send_line("+250");
    run.send_line("TOGGLE {oops");
    run.send_line("+abc");
    run.send_line("TOGGLE");
    run.wait_for("the TOGGLE step", |l| l.contains("TOGGLE") && l.contains("active"));
    run.wait_for("the clock step", |l| l.contains("+250 ms"));
    run.send_line("quit");
    assert_eq!(run.exit_code(), 0);
}

#[test]
fn live_events_option_runs_a_script_and_exits_after_the_duration() {
    xsm()
        .args([
            "inspect",
            &toggle(),
            "--live",
            "--port",
            "0",
            "-e",
            "TOGGLE,TOGGLE",
            "--duration",
            "0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("live inspector: http://127.0.0.1:"))
        .stdout(predicate::str::contains("TOGGLE"));
}

#[test]
fn live_refuses_a_non_loopback_host_without_a_token() {
    xsm()
        .args(["inspect", &toggle(), "--live", "--host", "0.0.0.0", "--port", "0"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("non-loopback").and(predicate::str::contains("--token")));
}

#[test]
fn live_rejects_a_bad_duration_and_a_machine_that_does_not_build() {
    xsm().args(["inspect", &toggle(), "--live", "--duration", "-1"]).assert().failure();
    xsm()
        .args(["inspect", &toggle(), "--live", "--port", "0", "--duration", "nan"])
        .assert()
        .code(2);
    xsm()
        .args(["inspect"])
        .arg(fixture("invalid.json"))
        .args(["--live", "--port", "0"])
        .assert()
        .code(1);
}

// ----------------------------------------------------------------------------- xsm replay

/// Record `TOGGLE,TOGGLE` on the toggle machine into `dir`/rec.jsonl.
fn recording(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("rec.jsonl").display().to_string();
    xsm()
        .args(["simulate", &toggle(), "-e", "TOGGLE,TOGGLE", "--record", &path])
        .assert()
        .success();
    path
}

#[test]
fn replay_prints_the_recording() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording(&dir);
    xsm()
        .args(["replay", &path])
        .assert()
        .success()
        .stdout(predicate::str::contains("@xstate.actor"))
        .stdout(predicate::str::contains("TOGGLE"))
        .stdout(predicate::str::contains("\"active\""));
}

#[test]
fn replay_live_serves_the_recording() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording(&dir);
    let mut run = Running::spawn(&["replay", &path, "--live", "--port", "0", "--duration", "60"]);
    let endpoint = run.endpoint();
    let line = run.wait_for("the replay summary", |l| l.starts_with("replayed "));
    let count: usize = line.split(' ').nth(1).unwrap().parse().unwrap();
    let messages = endpoint.messages();
    assert_eq!(messages.len(), count);
    assert!(count >= 5, "{messages:?}");
    assert_eq!(event_names(&messages).iter().filter(|e| *e == "TOGGLE").count(), 2);
    assert_eq!(types_of(&messages)[0], "@xstate.actor");
}

#[test]
fn replay_live_exits_zero_after_its_duration() {
    let dir = tempfile::tempdir().unwrap();
    let path = recording(&dir);
    xsm()
        .args(["replay", &path, "--live", "--port", "0", "--duration", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("replayed "));
}

#[test]
fn replay_refuses_what_is_not_a_recording() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("trace.jsonl");
    std::fs::write(&other, "{\"name\":\"span\"}\n").unwrap();
    xsm()
        .args(["replay"])
        .arg(&other)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a recording"));
    xsm()
        .args(["replay"])
        .arg(&other)
        .args(["--live", "--port", "0", "--duration", "0"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a recording"));
    xsm().args(["replay"]).arg(dir.path().join("missing.jsonl")).assert().code(1);
    xsm().args(["replay", "--speed", "-1"]).arg(&other).assert().code(2);
}
