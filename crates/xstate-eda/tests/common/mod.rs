#![allow(dead_code, clippy::print_stderr)] // shared by several test crates; each uses a subset

//! Helpers shared by the integration tests: a minimal executor, the Python interpreter of the
//! sibling checkout, and small machines.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use serde_json::{Value, json};
use xstate_core::Machine;

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Run a future to completion on the current thread (no runtime needed).
pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park();
    }
}

pub(crate) fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
}

pub(crate) fn golden(name: &str) -> Value {
    let text =
        std::fs::read_to_string(golden_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The Python of the sibling checkout (or `XSM_PYTHON`) if it can import the library.
pub(crate) fn python() -> Option<PathBuf> {
    let candidate = std::env::var("XSM_PYTHON").map(PathBuf::from).ok().or_else(|| {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../PythonProjects/xstate-statemachine");
        [".venv/Scripts/python.exe", ".venv/bin/python"]
            .iter()
            .map(|r| checkout.join(r))
            .find(|p| p.exists())
    });
    let Some(py) = candidate.filter(|p| p.exists()) else {
        eprintln!("skipping cross-language test: no Python virtualenv found (set XSM_PYTHON)");
        return None;
    };
    let ok = Command::new(&py)
        .args(["-I", "-c", "import xstate_statemachine.eda"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!(
            "skipping cross-language test: {} cannot import xstate_statemachine",
            py.display()
        );
        return None;
    }
    Some(py)
}

/// Run `script` with `args`; stdout on success.
pub(crate) fn run_python(py: &Path, script: &str, args: &[&str]) -> String {
    let out = Command::new(py)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `counter`: `ADD` appends `n` to `seen` via the `record` action.
pub(crate) fn counter() -> Machine {
    Machine::from_value(&json!({
        "id": "counter",
        "initial": "on",
        "context": {"seen": []},
        "states": {"on": {"on": {"ADD": {"actions": "record"}}}}
    }))
    .unwrap_or_else(|e| panic!("{e}"))
}
