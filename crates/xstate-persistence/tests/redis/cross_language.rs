//! A Redis shared by Rust and the Python library: each runtime writes, the other reads, and the
//! two take each other's locks. Skipped (with a note) when Redis, the Python virtualenv or the
//! `redis` Python client is not available; override the interpreter with `XSM_PYTHON`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use xstate_core::snapshot::Deadline;
use xstate_persistence::idempotency::InboxStore;
use xstate_persistence::log::TransitionLogStore;
use xstate_persistence::{SaveOptions, StateStore, StoreError};

use super::{Ns, deadline, ok, record};

const KEY: &str = "Ordér:42 日本 😀";
const SNAPSHOT: &str = r#"{"v":4,"note":"héllo 日本 😀"}"#;
const SCOPE: &str = "tenant/machine/instance";
const IMPORTS: &str = "
import sys, json, time
from xstate_statemachine.contrib.redis import RedisStore, RedisInbox, RedisLog
from xstate_statemachine.persistence import Deadline
from xstate_statemachine.persistence.log import TransitionRecord
url, prefix, key = sys.argv[1:4]
store = RedisStore(url, prefix=prefix)
inbox = RedisInbox(url, prefix=prefix)
log = RedisLog(url, prefix=prefix)
scope = 'tenant/machine/instance'
";

/// Python writes: a record (twice, so version 2) with two deadlines, a key containing `|`, two
/// inbox entries and two log records.
const PY_WRITE: &str = "
snap = '{\"v\":4,\"note\":\"h\\u00e9llo \\u65e5\\u672c \\U0001F600\"}'
dls = [Deadline('m.a', 3, 1700000000.5, 1500, 'after.1500.m.a'),
       Deadline('m.b', 4, 1700000900.25, 900000, 'after.900000.m.b')]
store.save(key, snap, machine_version='2.1.0', deadlines=dls)
print(store.save(key, snap, machine_version='2.1.0', deadlines=dls))
store.save('py|pipe', '{}', deadlines=[Deadline('m.p', 1, 1700000100.0, 10, 'after.10.m.p')])
inbox.claim(scope, 'idem-1', 'fp-py', ttl_s=3600.0)
inbox.mark(scope, 'idem-1', '{\"status\": 200}', ttl_s=3600.0)
inbox.claim(scope, 'idem-2', 'fp-2', ttl_s=None)
for seq in (1, 2):
    log.append(TransitionRecord(machine_id=key, seq=seq, ts=100.0 + seq, event_type='GO',
                                event_payload={'n': seq, 't': '\\u00e9'}, from_states=('m.a',),
                                to_states=('m.b',), actions=('act',)))
";

/// Python reads what Rust wrote and prints it as ASCII JSON.
const PY_READ: &str = "
rec = store.load(key)
print(json.dumps({
    'version': rec.version,
    'snapshot': rec.snapshot,
    'machine_version': rec.machine_version,
    'deadlines': [d.to_dict() for d in rec.deadlines],
    'keys': store.list_keys(),
    'due': [k for k, _ in store.due_keys(1700001000.0)],
    'idem1_duplicate': not inbox.claim(scope, 'idem-1', 'other', ttl_s=None),
    'idem2_released': inbox.get(scope, 'idem-2') is None,
    'idem3': inbox.get(scope, 'idem-3') is not None and inbox.get(scope, 'idem-3').receipt_json,
    'log': [r.to_dict() for r in log.read(key)],
}))
";

/// Python takes the lock on `locked:py`, announces it, and holds it until stdin gets a line.
const PY_HOLD_LOCK: &str = "
with store.lock('locked:py', timeout=1.0):
    print('locked', flush=True)
    sys.stdin.readline()
";

/// Python tries the lock on `locked:rust` for 200 ms.
const PY_TRY_LOCK: &str = "
from xstate_statemachine.exceptions import LockTimeoutError
try:
    with store.lock('locked:rust', timeout=0.2):
        print('acquired')
except LockTimeoutError:
    print('timeout')
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
    prefix: String,
}

impl Py {
    fn command(&self, script: &str) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("-I").arg("-c").arg(format!("{IMPORTS}{script}")).args([
            &self.url,
            &self.prefix,
            KEY,
        ]);
        cmd
    }

    fn run(&self, script: &str) -> String {
        let out = self.command(script).output().unwrap_or_else(|e| panic!("python: {e}"));
        assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
}

/// The interpreter, when it can import the library and its Redis client; `None` means "skip".
fn usable_python(ns: &Ns) -> Option<Py> {
    let Some(exe) = python() else {
        eprintln!("skipping Redis cross-language test: no Python virtualenv (set XSM_PYTHON)");
        return None;
    };
    let py = Py { exe, url: ns.url.clone(), prefix: ns.prefix.clone() };
    let probe = py.command("").output().ok()?;
    if probe.status.success() {
        return Some(py);
    }
    eprintln!(
        "skipping Redis cross-language test: {}",
        String::from_utf8_lossy(&probe.stderr).lines().last().unwrap_or("python import failed")
    );
    None
}

fn dls_written_by_python() -> Vec<Deadline> {
    vec![deadline("m.a", 3, 1_700_000_000.5, 1500), deadline("m.b", 4, 1_700_000_900.25, 900_000)]
}

fn expect_python_data(ns: &Ns) {
    let store = ns.store();
    let rec = ok(store.load(KEY)).expect("Rust sees the record Python wrote");
    assert_eq!(
        (rec.version, rec.snapshot.as_str(), rec.machine_version.as_str()),
        (2, SNAPSHOT, "2.1.0")
    );
    assert_eq!(rec.deadlines, dls_written_by_python());
    assert!(rec.updated_at > 1.6e9);
    let mut keys = vec![KEY.to_owned(), "py|pipe".to_owned()];
    keys.sort();
    assert_eq!(ok(store.list_keys("", 10)), keys);
    assert_eq!(
        ok(store.due_keys(1_700_000_200.0, 10)),
        [KEY, "py|pipe"],
        "Python's index, Rust's read"
    );

    let inbox = ns.inbox();
    let done = ok(inbox.get(SCOPE, "idem-1")).expect("Python's receipt");
    assert_eq!(
        (done.fingerprint.as_str(), done.receipt_json.as_deref()),
        ("fp-py", Some(r#"{"status": 200}"#))
    );
    assert!(done.expires_at.is_some_and(|t| t > 1.6e9));
    let flying = ok(inbox.get(SCOPE, "idem-2")).expect("Python's in-flight claim");
    assert_eq!((flying.fingerprint.as_str(), flying.receipt_json), ("fp-2", None));
    assert!(!ok(inbox.claim(SCOPE, "idem-1", "other", None)), "Python's claim holds in Rust");

    let log = ns.log();
    let rows = ok(log.read(KEY, 0, 10));
    assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(rows[1].event_payload["t"], "é");
    assert_eq!(ok(log.next_seq(KEY)), 3);
}

fn write_from_rust(ns: &Ns) {
    let store = ns.store();
    let dls = [deadline("m.c", 9, 1_700_000_050.0, 5)];
    let opts = SaveOptions { expected_version: Some(2), machine_version: "2.2.0", deadlines: &dls };
    assert_eq!(ok(store.save(KEY, SNAPSHOT, &opts)), 3, "continues Python's version counter");
    let inbox = ns.inbox();
    assert!(ok(inbox.claim(SCOPE, "idem-3", "fp-rust", Some(7200.0))));
    ok(inbox.mark(SCOPE, "idem-3", r#"{"status": 201}"#, Some(7200.0)));
    ok(inbox.release(SCOPE, "idem-2"));
    let appended = ok(ns.log().append_next(record(KEY, 0, 103.0)));
    assert_eq!(appended.seq, 3);
}

fn expect_rust_data_in_python(seen: &Value) {
    assert_eq!(seen["version"], 3);
    assert_eq!(seen["snapshot"], SNAPSHOT);
    assert_eq!(seen["machine_version"], "2.2.0");
    let dl = &seen["deadlines"];
    assert_eq!(dl.as_array().map(Vec::len), Some(1), "Rust removed Python's old deadline members");
    assert_eq!(dl[0]["state_id"], "m.c");
    assert_eq!(
        (dl[0]["entry_seq"].as_u64(), dl[0]["due_at_wall"].as_f64()),
        (Some(9), Some(1_700_000_050.0))
    );
    assert_eq!(seen["due"], serde_json::json!([KEY, "py|pipe"]));
    assert!(seen["keys"].as_array().is_some_and(|k| k.iter().any(|v| v == KEY)));
    assert_eq!(seen["idem1_duplicate"], true);
    assert_eq!(seen["idem2_released"], true);
    assert_eq!(seen["idem3"], r#"{"status": 201}"#);
    let log = seen["log"].as_array().expect("log");
    assert_eq!(
        log.iter().map(|r| r["seq"].as_u64()).collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3)]
    );
    assert_eq!(log[2]["machine_id"], KEY);
    assert_eq!(log[2]["actor"], "alice");
}

#[test]
fn python_writes_rust_reads_then_rust_writes_python_reads() {
    let ns = ns!("xlang");
    let Some(py) = usable_python(&ns) else { return };
    assert_eq!(py.run(PY_WRITE), "2");
    expect_python_data(&ns);
    write_from_rust(&ns);
    let seen: Value = ok(serde_json::from_str(&py.run(PY_READ)));
    expect_rust_data_in_python(&seen);
}

#[test]
fn the_two_runtimes_exclude_each_other_with_the_same_lock_keys() {
    let ns = ns!("xlock");
    let Some(py) = usable_python(&ns) else { return };
    let store = ns.store();

    // Rust holds `locked:rust`; Python times out; once released Python gets it.
    let guard = ok(store.lock("locked:rust", Duration::ZERO));
    assert_eq!(py.run(PY_TRY_LOCK), "timeout");
    drop(guard);
    assert_eq!(py.run(PY_TRY_LOCK), "acquired");

    // Python holds `locked:py`; Rust times out; once Python lets go Rust gets it.
    let mut child = py
        .command(PY_HOLD_LOCK)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("python: {e}"));
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    ok(stdout.read_line(&mut line));
    assert_eq!(line.trim(), "locked");
    assert!(matches!(
        store.lock("locked:py", Duration::from_millis(100)),
        Err(StoreError::LockTimeout { .. })
    ));
    ok(child.stdin.take().expect("piped stdin").write_all(b"\n"));
    assert!(ok(child.wait()).success());
    drop(ok(store.lock("locked:py", Duration::from_secs(1))));
}
