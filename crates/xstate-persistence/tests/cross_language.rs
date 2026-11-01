#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
//! Cross-language checks: records written by the Python library load here and vice versa
//! (ADR-0003). Skipped when the Python checkout's virtualenv is not available (override the
//! interpreter with `XSM_PYTHON`).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use xstate_core::snapshot::Deadline;
use xstate_persistence::{FileStore, FileStoreOptions, SaveOptions, StateStore};

const KEY: &str = "Ordér:42 日本 😀";
const SNAPSHOT: &str = r#"{"v":4,"note":"héllo 日本 😀 \"quoted\" \\ \n tab\t","n":[1,2.5,null]}"#;
const MACHINE_VERSION: &str = "2.1.0";

/// Python: save the same record twice (so the version is 2) into a `file` or `sqlite` store.
const PY_WRITE: &str = r#"
import sys
from xstate_statemachine.persistence import FileStore, SQLiteStore, Deadline
kind, path, key, snap, mv = sys.argv[1:6]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
deadlines = [
    Deadline("m.a", 3, 1700000000.5, 1500, "after.1500.m.a"),
    Deadline("m.b", 4, 1700000900.25, 900000, "after.900000.m.b"),
]
store.save(key, snap, machine_version=mv, deadlines=deadlines)
print(store.save(key, snap, machine_version=mv, deadlines=deadlines))
"#;

/// Python: load a key and print what it sees as ASCII JSON.
const PY_READ: &str = r#"
import sys, json
from xstate_statemachine.persistence import FileStore, SQLiteStore
kind, path, key = sys.argv[1:4]
store = FileStore(path) if kind == "file" else SQLiteStore(path)
rec = store.load(key)
print(json.dumps({
    "version": rec.version,
    "snapshot": rec.snapshot,
    "machine_version": rec.machine_version,
    "deadlines": [d.to_dict() for d in rec.deadlines],
    "keys": store.list_keys(),
}))
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

fn run(py: &Path, script: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(py)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// The interpreter, if it exists and can import the library; `None` means "skip".
fn usable_python() -> Option<PathBuf> {
    let Some(py) = python() else {
        eprintln!("skipping cross-language test: no Python virtualenv found (set XSM_PYTHON)");
        return None;
    };
    match run(&py, "import xstate_statemachine.persistence", &[]) {
        Ok(_) => Some(py),
        Err(e) => {
            eprintln!("skipping cross-language test: cannot import xstate_statemachine: {e}");
            None
        }
    }
}

fn deadlines() -> Vec<Deadline> {
    let dl = |state: &str, seq, due, delay: i64| Deadline {
        state_id: state.to_owned(),
        entry_seq: seq,
        due_at_wall: due,
        delay_ms: json!(delay),
        event_type: format!("after.{delay}.{state}"),
    };
    vec![dl("m.a", 3, 1_700_000_000.5, 1500), dl("m.b", 4, 1_700_000_900.25, 900_000)]
}

fn save_opts(deadlines: &[Deadline]) -> SaveOptions<'_> {
    SaveOptions { machine_version: MACHINE_VERSION, deadlines, ..SaveOptions::default() }
}

/// Python writes, Rust loads; Rust writes on top, Python loads; versions keep counting.
fn round_trip(
    kind: &str,
    open: impl Fn(&Path) -> Box<dyn StateStore>,
    target: impl Fn(&Path) -> PathBuf,
) {
    let Some(py) = usable_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let location = target(dir.path());
    let location_str = location.to_str().unwrap();

    // Python -> Rust
    let version =
        run(&py, PY_WRITE, &[kind, location_str, KEY, SNAPSHOT, MACHINE_VERSION]).unwrap();
    assert_eq!(version, "2");
    let store = open(&location);
    let rec = store.load(KEY).unwrap().expect("Rust sees the record Python wrote");
    assert_eq!(rec.key, KEY);
    assert_eq!(rec.snapshot, SNAPSHOT);
    assert_eq!(rec.version, 2);
    assert_eq!(rec.machine_version, MACHINE_VERSION);
    assert_eq!(rec.deadlines, deadlines());
    assert!(rec.updated_at > 1.6e9);
    assert_eq!(store.list_keys("", 10).unwrap(), [KEY]);

    // Rust -> Python, continuing Python's version counter
    let again = deadlines();
    let opts = SaveOptions { expected_version: Some(2), ..save_opts(&again) };
    assert_eq!(store.save(KEY, SNAPSHOT, &opts).unwrap(), 3);
    assert_eq!(store.save("rust:only", "{\"from\":\"rust\"}", &save_opts(&[])).unwrap(), 1);

    let seen: Value =
        serde_json::from_str(&run(&py, PY_READ, &[kind, location_str, KEY]).unwrap()).unwrap();
    assert_eq!(seen["version"], 3);
    assert_eq!(seen["snapshot"], SNAPSHOT);
    assert_eq!(seen["machine_version"], MACHINE_VERSION);
    assert_eq!(seen["deadlines"][0]["delay_ms"], 1500);
    assert_eq!(seen["deadlines"][0]["entry_seq"], 3);
    assert_eq!(seen["deadlines"][1]["due_at_wall"], 1_700_000_900.25);
    assert_eq!(seen["deadlines"][1]["event_type"], "after.900000.m.b");
    assert_eq!(seen["keys"], json!(["Ordér:42 日本 😀", "rust:only"]));
    let only: Value =
        serde_json::from_str(&run(&py, PY_READ, &[kind, location_str, "rust:only"]).unwrap())
            .unwrap();
    assert_eq!(only["snapshot"], "{\"from\":\"rust\"}");
    assert_eq!(only["deadlines"], json!([]));
}

#[test]
fn file_store_round_trips_with_python() {
    round_trip("file", |p| Box::new(FileStore::open(p).unwrap()), |d| d.join("records"));
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_store_round_trips_with_python() {
    round_trip(
        "sqlite",
        |p| Box::new(xstate_persistence::SqliteStore::open(p).unwrap()),
        |d| d.join("shared.sqlite3"),
    );
}

/// The bytes Rust writes equal the bytes Python writes, apart from the timestamp.
#[test]
fn file_records_are_byte_identical_to_pythons() {
    let Some(py) = usable_python() else { return };
    let (py_dir, rs_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let py_records = py_dir.path().join("r");
    run(&py, PY_WRITE, &["file", py_records.to_str().unwrap(), KEY, SNAPSHOT, MACHINE_VERSION])
        .unwrap();
    let store =
        FileStore::open_with(rs_dir.path().join("r"), FileStoreOptions::default().fsync(false))
            .unwrap();
    store.save(KEY, SNAPSHOT, &save_opts(&deadlines())).unwrap();
    store.save(KEY, SNAPSHOT, &save_opts(&deadlines())).unwrap();

    let name = format!("{}.xsm.json", xstate_persistence::file_store::file_stem(KEY));
    assert!(name.starts_with("%4frd%c3%a9r"), "upper case is escaped, hex is lower case: {name}");
    let python_text =
        std::fs::read_to_string(py_records.join(&name)).expect("Python used the same file name");
    let rust_text = std::fs::read_to_string(rs_dir.path().join("r").join(&name)).unwrap();
    // blank out the timestamp, the only field that legitimately differs
    let blank = |text: &str| {
        let from = text.find("\"updated_at\":").unwrap();
        let to = text.find(",\"deadlines\"").unwrap();
        format!("{}\"updated_at\":T{}", &text[..from], &text[to..])
    };
    assert_eq!(blank(&rust_text), blank(&python_text));
}

/// Python and Rust create the same SQLite schema, statement text included.
#[test]
#[cfg(feature = "sqlite")]
fn sqlite_schemas_are_identical_to_pythons() {
    let Some(py) = usable_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (py_db, rs_db) = (dir.path().join("py.db"), dir.path().join("rs.db"));
    run(&py, PY_WRITE, &["sqlite", py_db.to_str().unwrap(), "k", "{}", "1"]).unwrap();
    drop(xstate_persistence::SqliteStore::open(&rs_db).unwrap());

    let schema = |db: &Path| -> Vec<(String, String, Option<String>)> {
        let c = rusqlite::Connection::open(db).unwrap();
        let mut stmt = c
            .prepare("SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_stat%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let version = |db: &Path| -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row("SELECT version FROM xsm_schema", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(schema(&rs_db), schema(&py_db));
    assert_eq!(version(&rs_db), version(&py_db));
}
