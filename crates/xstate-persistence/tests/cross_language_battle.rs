#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
//! Cross-language battle additions for the §6.2 matrix: large snapshots, exotic unicode keys,
//! and an explicit version-race / conflict across the two runtimes, for `FileStore` and
//! `SqliteStore`. Redis and PostgreSQL interop is already covered in `redis/cross_language.rs`
//! and `postgres/cross_language.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use xstate_persistence::{FileStore, FileStoreOptions, SaveOptions, StateStore, StoreError};

// The snapshot is generated inside the script: it is far too large for a Windows command line.
const PY_WRITE_LARGE: &str = r#"
import sys
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path, key = sys.argv[1:4]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
snap = '{"v":4,"blob":"' + "x" * 400_000 + '"}'
print(store.save(key, snap, machine_version="big"))
"#;

const PY_READ_LARGE: &str = r#"
import sys, json
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path, key = sys.argv[1:4]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
rec = store.load(key)
print(len(rec.snapshot), rec.version, rec.machine_version)
"#;

const PY_WRITE_UNICODE: &str = r#"
import sys, json
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path = sys.argv[1], sys.argv[2]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
keys = ["café", "あア\U0001F600", "שלום", "e\u0301", "\U0001F469‍\U0001F4BB", "العربية"]
for i, k in enumerate(keys):
    print(store.save(k, '{"i":' + str(i) + '}', machine_version=""))
"#;

const PY_READ_UNICODE: &str = r#"
import sys, json
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path = sys.argv[1], sys.argv[2]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
print(json.dumps(sorted(store.list_keys()), ensure_ascii=True))
"#;

const PY_WRITE_TWICE: &str = r#"
import sys
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path, key = sys.argv[1:4]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
store.save(key, '{"v":4}')
print(store.save(key, '{"v":4}'))
"#;

const PY_READ_VERSION: &str = r#"
import sys
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path, key = sys.argv[1:4]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
print(store.load(key).version)
"#;

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

fn run(py: &Path, script: &str, args: &[&str]) -> String {
    let out = Command::new(py)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("python: {e}"));
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn usable_python() -> Option<PathBuf> {
    let Some(py) = python() else {
        eprintln!("skipping cross-language battle test: no Python virtualenv (set XSM_PYTHON)");
        return None;
    };
    run(&py, "import xstate_statemachine.persistence", &[]);
    Some(py)
}

fn target(kind: &str, dir: &Path) -> PathBuf {
    if kind == "file" { dir.join("records") } else { dir.join("shared.sqlite3") }
}

fn open_store(kind: &str, location: &Path) -> Box<dyn StateStore> {
    #[cfg(feature = "sqlite")]
    if kind == "sqlite" {
        return Box::new(xstate_persistence::SqliteStore::open(location).unwrap());
    }
    let _ = kind;
    Box::new(FileStore::open_with(location, FileStoreOptions::default().fsync(false)).unwrap())
}

#[test]
fn large_snapshot_round_trips_with_python() {
    let Some(py) = usable_python() else { return };
    for kind in ["file", "sqlite"] {
        if kind == "sqlite" && cfg!(not(feature = "sqlite")) {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let location = target(kind, dir.path());
        let loc = location.to_str().unwrap();
        // Python -> Rust
        assert_eq!(run(&py, PY_WRITE_LARGE, &[kind, loc, "big:1"]), "1");
        let store = open_store(kind, &location);
        let rec = store.load("big:1").unwrap().expect("saved");
        assert_eq!(rec.snapshot.len(), 400_000 + r#"{"v":4,"blob":""#.len() + 2);
        assert!(rec.snapshot.starts_with(r#"{"v":4,"blob":"#));
        assert!(rec.snapshot.ends_with(r#""}"#));
        // Rust -> Python
        let big = format!(r#"{{"v":4,"blob":"{}"}}"#, "y".repeat(400_000));
        let opts = SaveOptions { machine_version: "big", ..SaveOptions::default() };
        store.save("big:2", &big, &opts).unwrap();
        let seen = run(&py, PY_READ_LARGE, &[kind, loc, "big:2"]);
        assert_eq!(seen, format!("{} 1 big", big.len()));
    }
}

#[test]
fn unicode_keys_round_trip_with_python() {
    let Some(py) = usable_python() else { return };
    for kind in ["file", "sqlite"] {
        if kind == "sqlite" && cfg!(not(feature = "sqlite")) {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let location = target(kind, dir.path());
        let loc = location.to_str().unwrap();
        run(&py, PY_WRITE_UNICODE, &[kind, loc]);
        let store = open_store(kind, &location);
        let keys = store.list_keys("", 100).unwrap();
        assert_eq!(keys.len(), 6);
        assert!(keys.contains(&"café".to_owned()));
        assert!(keys.contains(&"あア😀".to_owned()));
        assert!(keys.contains(&"שלום".to_owned()));
        assert!(keys.contains(&"e\u{301}".to_owned()));
        assert!(keys.contains(&"👩\u{200d}💻".to_owned()));
        assert!(keys.contains(&"العربية".to_owned()));
        // Python sees the same keys (as escaped ASCII JSON, so compare through a decode).
        let seen: Vec<String> =
            serde_json::from_str(&run(&py, PY_READ_UNICODE, &[kind, loc])).unwrap();
        assert_eq!(seen.len(), 6);
        assert!(seen.contains(&"あア😀".to_owned()));
    }
}

#[test]
fn a_stale_version_races_to_a_conflict_across_the_runtimes() {
    let Some(py) = usable_python() else { return };
    for kind in ["file", "sqlite"] {
        if kind == "sqlite" && cfg!(not(feature = "sqlite")) {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let location = target(kind, dir.path());
        let loc = location.to_str().unwrap();
        assert_eq!(run(&py, PY_WRITE_TWICE, &[kind, loc, "race:1"]), "2");
        let store = open_store(kind, &location);
        // Rust's stale view (version 1) loses the race: a conflict, nothing written.
        let stale = SaveOptions { expected_version: Some(1), ..SaveOptions::default() };
        match store.save("race:1", "{}", &stale) {
            Err(StoreError::Conflict { expected: 1, actual: Some(2), .. }) => {}
            other => panic!("expected a conflict, got {other:?}"),
        }
        // A correct version continues Python's counter.
        let current = SaveOptions { expected_version: Some(2), ..SaveOptions::default() };
        assert_eq!(store.save("race:1", "{}", &current).unwrap(), 3);
        assert_eq!(run(&py, PY_READ_VERSION, &[kind, loc, "race:1"]), "3");
    }
}
