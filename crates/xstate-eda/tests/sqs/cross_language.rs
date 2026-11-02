//! SQS shared with the Python library: each runtime publishes, the other consumes, on a standard
//! and on a FIFO queue. Skipped (with a note) when the Python virtualenv or `boto3` is missing;
//! override the interpreter with `XSM_PYTHON`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use xstate_eda::SyncBrokerAdapter;

use super::*;

const SUBJECTS: [&str; 4] = ["order:1", "order-2", "key_3", "x/4"];

const PY_HEAD: &str = "
import sys, json, asyncio, boto3
from xstate_statemachine.contrib.brokers.sqs import SqsBroker
from xstate_statemachine.eda.envelope import Envelope
url, topic, subjects = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
client = boto3.client('sqs', region_name='us-east-1', endpoint_url=url,
                      aws_access_key_id='test', aws_secret_access_key='test')
";

const PY_PUBLISH: &str = "
async def main():
    broker = SqsBroker(client=client)
    for i, subject in enumerate(subjects):
        await broker.publish(topic, Envelope.new(type='xsm.order.GO', subject=subject,
                             data={'n': i, 't': '\\u00e9'}, correlationid='corr-' + str(i)))
asyncio.run(main())
";

const PY_CONSUME: &str = "
async def main():
    broker = SqsBroker(client=client)
    got = []
    async for d in broker.subscribe(topic, timeout=1.5):
        attempt = d.envelope.attempt
        got.append({'id': d.envelope.id, 'type': d.envelope.type, 'subject': d.envelope.subject,
                    'data': d.envelope.data, 'attempt': attempt() if callable(attempt) else attempt})
        await d.ack()
    print(json.dumps({'got': got}))
asyncio.run(main())
";

fn python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("XSM_PYTHON") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../PythonProjects/xstate-statemachine");
    [".venv/Scripts/python.exe", ".venv/bin/python"]
        .iter()
        .map(|rel| checkout.join(rel))
        .find(|p| p.exists())
}

struct Py {
    exe: PathBuf,
    endpoint: String,
}

impl Py {
    fn command(&self, script: &str, topic: &str, subjects: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("-I")
            .arg("-c")
            .arg(format!("{PY_HEAD}{script}"))
            .args([&self.endpoint, topic])
            .arg(json!(subjects).to_string());
        cmd
    }

    fn run(&self, script: &str, topic: &str, subjects: &[&str]) -> Value {
        let out = self
            .command(script, topic, subjects)
            .output()
            .unwrap_or_else(|e| panic!("python: {e}"));
        assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
}

fn usable_python(qs: &Qs) -> Option<Py> {
    let Some(exe) = python() else {
        eprintln!("skipping SQS cross-language test: no Python virtualenv (set XSM_PYTHON)");
        return None;
    };
    let py = Py { exe, endpoint: qs.endpoint.clone() };
    let probe = py.command("", "t", &[]).output().ok()?;
    if probe.status.success() {
        return Some(py);
    }
    eprintln!(
        "skipping SQS cross-language test: {}",
        String::from_utf8_lossy(&probe.stderr).lines().last().unwrap_or("python import failed")
    );
    None
}

/// Python publishes and Rust consumes, then the other way round, on `queue`.
fn round_trip(qs: &Qs, py: &Py, queue: &str, ordered: bool) {
    let rust = qs.broker(&SqsOptions::new());

    // Python -> Rust
    py.run(PY_PUBLISH, queue, &SUBJECTS);
    let mut got = Vec::new();
    while let Some(d) = ok(rust.poll(queue, Some(Duration::from_millis(1500)))) {
        assert_eq!(d.envelope.attempt(), 0);
        ok(rust.ack(&d));
        got.push(d.envelope);
    }
    assert_eq!(got.len(), SUBJECTS.len());
    if !ordered {
        got.sort_by_key(|e| e.data["n"].as_i64().unwrap_or(-1));
    }
    for (i, subject) in SUBJECTS.iter().enumerate() {
        let e = &got[i];
        assert_eq!(e.subject.as_deref(), Some(*subject));
        assert_eq!((e.ty.as_str(), &e.data), ("xsm.order.GO", &json!({"n": i, "t": "é"})));
        assert_eq!(e.correlationid.as_deref(), Some(format!("corr-{i}").as_str()));
    }
    eventually("Python-published messages deleted", || qs.counts(queue) == (0, 0));

    // Rust -> Python
    let sent: Vec<Envelope> =
        SUBJECTS.iter().zip(0..).map(|(subject, n)| envelope(subject, n)).collect();
    for e in &sent {
        ok(rust.publish(queue, e));
    }
    let seen = py.run(PY_CONSUME, queue, &SUBJECTS);
    let mut consumed: Vec<Value> = seen["got"].as_array().cloned().unwrap_or_default();
    assert_eq!(consumed.len(), sent.len());
    if !ordered {
        consumed.sort_by_key(|c| c["data"]["n"].as_i64().unwrap_or(-1));
    }
    for (i, e) in sent.iter().enumerate() {
        assert_eq!(consumed[i]["id"], e.id.as_str());
        assert_eq!(consumed[i]["type"], e.ty.as_str());
        assert_eq!(consumed[i]["subject"], e.subject.as_deref().unwrap_or_default());
        assert_eq!(consumed[i]["data"], e.data);
        assert_eq!(consumed[i]["attempt"], 0);
    }
    eventually("Rust-published messages deleted by Python", || qs.counts(queue) == (0, 0));
}

#[test]
fn python_and_rust_share_a_standard_queue_in_both_directions() {
    let qs = qs!("xlang");
    let Some(py) = usable_python(&qs) else { return };
    round_trip(&qs, &py, &qs.topic, false);
}

#[test]
fn python_and_rust_share_a_fifo_queue_in_both_directions() {
    let qs = qs!("xfifo");
    let Some(py) = usable_python(&qs) else { return };
    let fifo = format!("{}.fifo", qs.topic);
    qs.create(&fifo, true);
    round_trip(&qs, &py, &fifo, false);
}

#[test]
fn a_republished_fifo_envelope_is_deduplicated_after_python_consumed_it() {
    let qs = qs!("xdedup");
    let Some(py) = usable_python(&qs) else { return };
    let fifo = format!("{}.fifo", qs.topic);
    qs.create(&fifo, true);
    let rust = qs.broker(&SqsOptions::new());
    let sent = envelope("order-1", 1);
    ok(rust.publish(&fifo, &sent));
    // Python consumes it, then the same envelope id is published again by Rust: SQS drops it
    let seen = py.run(PY_CONSUME, &fifo, &SUBJECTS);
    assert_eq!(seen["got"].as_array().map(Vec::len), Some(1));
    ok(rust.publish(&fifo, &sent));
    assert_eq!(qs.counts(&fifo), (0, 0), "the duplicate window dropped the re-publish");
}
