//! RabbitMQ shared with the Python library: each runtime publishes, the other consumes. Skipped
//! (with a note) when the Python virtualenv or `aio-pika` is missing; override the interpreter
//! with `XSM_PYTHON`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use xstate_eda::SyncBrokerAdapter;

use super::*;

const SUBJECTS: [&str; 4] = ["order:1", "order.2 x", "ключ-3", "日本/4"];

const PY_HEAD: &str = "
import sys, json, asyncio
from xstate_statemachine.contrib.brokers.rabbitmq import RabbitMQBroker
from xstate_statemachine.eda.envelope import Envelope
url, topic, subjects = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
";

/// Python publishes one envelope per subject and prints their ids.
const PY_PUBLISH: &str = "
async def main():
    broker = RabbitMQBroker(url=url)
    ids = []
    for i, subject in enumerate(subjects):
        env = Envelope.new(type='xsm.order.GO', subject=subject,
                           data={'n': i, 't': '\\u00e9'}, correlationid='corr-' + str(i))
        ids.append(env.id)
        await broker.publish(topic, env)
    print(json.dumps(ids))
    await broker.close()
asyncio.run(main())
";

/// Python consumes (and acks) everything on the topic.
const PY_CONSUME: &str = "
async def main():
    broker = RabbitMQBroker(url=url)
    got = []
    async for d in broker.subscribe(topic, timeout=1.5):
        attempt = d.envelope.attempt
        got.append({'id': d.envelope.id, 'type': d.envelope.type, 'subject': d.envelope.subject,
                    'data': d.envelope.data, 'attempt': attempt() if callable(attempt) else attempt})
        await d.ack()
    print(json.dumps(got))
    await broker.close()
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
    url: String,
}

impl Py {
    fn command(&self, script: &str, topic: &str, subjects: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("-I")
            .arg("-c")
            .arg(format!("{PY_HEAD}{script}"))
            .args([&self.url, topic])
            .arg(json!(subjects).to_string());
        cmd
    }

    fn run(&self, script: &str, topic: &str, subjects: &[&str]) -> Value {
        let out = self
            .command(script, topic, subjects)
            .output()
            .unwrap_or_else(|e| panic!("python: {e}"));
        assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
        ok(serde_json::from_slice(&out.stdout))
    }
}

fn usable_python(ns: &Ns) -> Option<Py> {
    let Some(exe) = python() else {
        eprintln!("skipping RabbitMQ cross-language test: no Python virtualenv (set XSM_PYTHON)");
        return None;
    };
    let py = Py { exe, url: ns.url.clone() };
    let probe = py.command("", "t", &[]).output().ok()?;
    if probe.status.success() {
        return Some(py);
    }
    eprintln!(
        "skipping RabbitMQ cross-language test: {}",
        String::from_utf8_lossy(&probe.stderr).lines().last().unwrap_or("python import failed")
    );
    None
}

#[test]
fn python_publishes_rust_consumes_then_rust_publishes_python_consumes() {
    let ns = ns!("xlang");
    let Some(py) = usable_python(&ns) else { return };
    let rust = ns.broker(&Ns::options());

    // Python -> Rust: the same durable queue, the same envelope
    let topic_a = ns.extra("a");
    let ids = py.run(PY_PUBLISH, &topic_a, &SUBJECTS);
    let mut got = Vec::new();
    while let Some(d) = ok(rust.poll(&topic_a, Some(Duration::from_millis(500)))) {
        assert_eq!(d.envelope.attempt(), 0);
        ok(rust.ack(&d));
        got.push(d.envelope);
    }
    assert_eq!(got.len(), SUBJECTS.len());
    for (i, subject) in SUBJECTS.iter().enumerate() {
        let e = &got[i]; // one queue: publish order
        assert_eq!(e.id.as_str(), ids[i].as_str().unwrap_or("?"));
        assert_eq!(e.subject.as_deref(), Some(*subject));
        assert_eq!((e.ty.as_str(), &e.data), ("xsm.order.GO", &json!({"n": i, "t": "é"})));
        assert_eq!(e.correlationid.as_deref(), Some(format!("corr-{i}").as_str()));
    }

    // what Python puts on the wire next to the envelope: the properties Rust writes too
    let topic_b = ns.extra("b");
    let ids = py.run(PY_PUBLISH, &topic_b, &SUBJECTS[..1]);
    let wire = ns.raw.get(&topic_b).unwrap_or_else(|| panic!("a message"));
    let p = &wire.properties;
    assert_eq!(Got::text(p.content_type().as_ref()).as_deref(), Some(CE_CONTENT_TYPE));
    assert_eq!(Got::text(p.message_id().as_ref()).as_deref(), ids[0].as_str());
    assert_eq!(Got::text(p.kind().as_ref()).as_deref(), Some("xsm.order.GO"));
    assert_eq!(*p.delivery_mode(), Some(2));
    assert_eq!(wire.header("ce_subject").as_deref(), Some(SUBJECTS[0]));
    assert!(!wire.redelivered);

    // Rust -> Python
    let sent: Vec<Envelope> =
        SUBJECTS.iter().zip(0..).map(|(subject, n)| envelope(subject, n)).collect();
    for e in &sent {
        ok(rust.publish(&topic_b, e));
    }
    let consumed = py.run(PY_CONSUME, &topic_b, &SUBJECTS);
    let consumed = consumed.as_array().unwrap_or_else(|| panic!("list"));
    assert_eq!(consumed.len(), sent.len());
    for (i, e) in sent.iter().enumerate() {
        assert_eq!(consumed[i]["id"], e.id.as_str(), "publish order");
        assert_eq!(consumed[i]["type"], e.ty.as_str());
        assert_eq!(consumed[i]["subject"], e.subject.as_deref().unwrap_or_default());
        assert_eq!(consumed[i]["data"], e.data);
        assert_eq!(consumed[i]["attempt"], 0);
    }
    eventually("both queues drained", || {
        ns.raw.count(&topic_a) == Some(0) && ns.raw.count(&topic_b) == Some(0)
    });
}
