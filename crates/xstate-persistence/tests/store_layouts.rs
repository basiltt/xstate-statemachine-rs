#![allow(clippy::unwrap_used, clippy::expect_used)]
//! On-disk layout tests for `FileStore` and `SqliteStore` (Python compatibility, ADR-0003).

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use xstate_core::snapshot::Deadline;
use xstate_persistence::file_store::{FORMAT_VERSION, decode_key, encode_key, file_stem};
use xstate_persistence::{FileStore, FileStoreOptions, SaveOptions, StateStore, StoreError};

fn dl(state: &str, seq: u64, due: f64, delay: i64) -> Deadline {
    Deadline {
        state_id: state.to_owned(),
        entry_seq: seq,
        due_at_wall: due,
        delay_ms: json!(delay),
        event_type: format!("after.{delay}.{state}"),
    }
}

fn quick(dir: &Path) -> FileStore {
    FileStore::open_with(dir, FileStoreOptions::default().fsync(false)).unwrap()
}

fn put(s: &dyn StateStore, key: &str, snapshot: &str) -> u64 {
    s.save(key, snapshot, &SaveOptions::default()).unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

// ------------------------------------------------------------------------------------
// FileStore
// ------------------------------------------------------------------------------------

/// Expected values come from running Python's `file_store._file_stem`.
const GOLDEN_STEMS: [(&str, &str); 14] = [
    ("order:42", "order%3a42"),
    ("Order", "%4frder"),
    ("a b", "a%20b"),
    ("con", "%5fcon"),
    ("CON", "%43%4f%4e"),
    ("com1", "%5fcom1"),
    ("lpt9", "%5flpt9"),
    ("nul", "%5fnul"),
    ("conx", "conx"),
    ("con.x", "con%2ex"),
    ("x.y", "x%2ey"),
    ("é", "%c3%a9"),
    ("日本", "%e6%97%a5%e6%9c%ac"),
    ("~x", "%7ex"),
];

#[test]
fn key_encoding_matches_python_golden_cases() {
    for (key, stem) in GOLDEN_STEMS {
        assert_eq!(encode_key(key), stem, "encode {key:?}");
        assert_eq!(file_stem(key), stem, "stem {key:?}");
        assert_eq!(decode_key(stem).as_deref(), Some(key), "decode {stem:?}");
    }
    assert_eq!(encode_key("_x-1"), "_x-1");
    assert_eq!(encode_key("a%b"), "a%25b");
}

#[test]
fn long_keys_are_hashed_like_python() {
    // Python: _file_stem("A" * 200) and _file_stem("é" * 100)
    assert_eq!(
        file_stem(&"A".repeat(200)),
        "~70d3bf8b0b9d83a61012f35fbf460c4207063fe31b4d6178390fe3b721cc03f7"
    );
    let stem = file_stem(&"é".repeat(100));
    assert_eq!(stem.len(), 65);
    assert!(stem.starts_with("~f42ec48e1e4b487e590"));
    // exactly at the limit stays verbatim
    assert_eq!(file_stem(&"a".repeat(200)), "a".repeat(200));
}

#[test]
fn decode_rejects_foreign_names() {
    assert_eq!(decode_key("%zz"), None);
    assert_eq!(decode_key("%4"), None);
    assert_eq!(decode_key("%ff"), None, "not valid UTF-8");
}

#[test]
fn record_layout_matches_python() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    let deadlines = [dl("m.a", 3, 1_700_000_000.5, 1500)];
    let opts =
        SaveOptions { machine_version: "v1", deadlines: &deadlines, ..SaveOptions::default() };
    s.save("order:42", r#"{"a":"é"}"#, &opts).unwrap();

    assert_eq!(names(dir.path()), ["order%3a42.lock", "order%3a42.xsm.json"]);
    let text = fs::read_to_string(dir.path().join("order%3a42.xsm.json")).unwrap();
    assert!(text.is_ascii(), "Python writes ensure_ascii JSON");
    // key order and separators as in Python's json.dump(rec, separators=(",", ":"))
    let bs = '\\';
    let expected_head = format!(
        r#"{{"format":1,"key":"order:42","machine_version":"v1","snapshot":"{{\"a\":\"{bs}u00e9\"}}","version":1,"updated_at":"#
    );
    assert!(text.starts_with(&expected_head), "{text}");
    assert!(text.ends_with(
        r#","deadlines":[{"state_id":"m.a","entry_seq":3,"due_at_wall":1700000000.5,"delay_ms":1500,"event_type":"after.1500.m.a"}]}"#
    ));
    assert_eq!(FORMAT_VERSION, 1);
}

#[test]
fn non_ascii_is_escaped_with_surrogate_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    put(&s, "k😀", "x\u{7f}😀\t");
    let text = fs::read_to_string(dir.path().join("k%f0%9f%98%80.xsm.json")).unwrap();
    let bs = '\\';
    let pair = format!("{bs}ud83d{bs}ude00");
    assert!(text.contains(&format!(r#""key":"k{pair}""#)), "{text}");
    assert!(text.contains(&format!(r#""snapshot":"x{bs}u007f{pair}{bs}t""#)), "{text}");
    assert_eq!(s.load("k😀").unwrap().unwrap().snapshot, "x\u{7f}😀\t");
}

#[test]
fn windows_device_names_and_long_keys_get_safe_file_names() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    put(&s, "con", "{}");
    let long = "é".repeat(100);
    put(&s, &long, "{}");
    let hashed = format!("{}.xsm.json", file_stem(&long));
    assert!(names(dir.path()).contains(&"%5fcon.xsm.json".to_owned()));
    assert!(names(dir.path()).contains(&hashed));
    assert_eq!(s.list_keys("", 10).unwrap(), ["con".to_owned(), long.clone()]);
    assert_eq!(s.load(&long).unwrap().unwrap().key, long);
}

#[test]
fn path_like_keys_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    for bad in ["..", ".", "a/b", "a\\b", "../etc/passwd", "/abs"] {
        assert!(matches!(s.load(bad), Err(StoreError::InvalidKey(_))), "{bad:?}");
        assert!(matches!(
            s.save(bad, "{}", &SaveOptions::default()),
            Err(StoreError::InvalidKey(_))
        ));
    }
    assert_eq!(names(dir.path()), Vec::<String>::new());
}

#[test]
fn saves_leave_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let s = FileStore::open(dir.path()).unwrap(); // default options: fsync on
    for i in 0..10 {
        put(&s, "k", &format!("{{\"i\":{i}}}"));
    }
    assert!(
        s.save("k", "{}", &SaveOptions { expected_version: Some(1), ..SaveOptions::default() })
            .is_err()
    );
    assert!(s.save("big", &"x".repeat(2 * 1024 * 1024), &SaveOptions::default()).is_err());
    s.delete("k").unwrap();
    put(&s, "k", "{}");
    let all = names(dir.path());
    assert!(all.iter().all(|n| !n.starts_with(".tmp-")), "{all:?}");
    assert_eq!(all, ["k.lock", "k.xsm.json"]);
}

#[test]
fn orphaned_temp_files_are_swept_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join(".tmp-0123456789abcdef.xsm.json");
    let fresh = dir.path().join(".tmp-fedcba9876543210.xsm.json");
    fs::write(&old, "{").unwrap();
    fs::write(&fresh, "{").unwrap();
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    fs::File::options().write(true).open(&old).unwrap().set_modified(hour_ago).unwrap();
    let s = quick(dir.path());
    assert!(!old.exists(), "a stale temp is removed");
    assert!(fresh.exists(), "a recent temp (live writer) is left alone");
    assert!(s.list_keys("", 10).unwrap().is_empty(), "temp files are never keys");
}

#[test]
fn list_keys_ignores_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    put(&s, "real", "{}");
    fs::write(dir.path().join("readme.txt"), "x").unwrap();
    fs::write(dir.path().join("ABC.xsm.json"), "{}").unwrap(); // not a name encode_key produces
    fs::write(dir.path().join("%zz.xsm.json"), "{}").unwrap();
    fs::write(dir.path().join("~deadbeef.xsm.json"), "not json").unwrap();
    assert_eq!(s.list_keys("", 10).unwrap(), ["real"]);
}

fn write_raw(dir: &Path, name: &str, value: &Value) {
    fs::write(dir.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn newer_format_is_refused_as_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    let rec = |format: Value| {
        json!({"format": format, "key": "k", "machine_version": "", "snapshot": "{}",
               "version": 1, "updated_at": 1.0, "deadlines": []})
    };
    write_raw(dir.path(), "k.xsm.json", &rec(json!(2)));
    let err = s.load("k").unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(ref m) if m.contains("newer")), "{err:?}");
    // saving over a newer-format record is refused too: the old record may not be clobbered
    assert!(matches!(put_err(&s, "k"), StoreError::Corrupt(_)));
    write_raw(dir.path(), "k.xsm.json", &rec(json!("x")));
    assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))));
    write_raw(dir.path(), "k.xsm.json", &rec(json!(0)));
    assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))));
    // records written before the `format` field existed are format 1
    let mut legacy = rec(json!(1));
    legacy.as_object_mut().unwrap().remove("format");
    write_raw(dir.path(), "k.xsm.json", &legacy);
    assert_eq!(s.load("k").unwrap().unwrap().version, 1);
}

fn put_err(s: &dyn StateStore, key: &str) -> StoreError {
    s.save(key, "{}", &SaveOptions::default()).unwrap_err()
}

#[test]
fn damaged_records_are_corrupt_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    let good = json!({"format": 1, "key": "k", "machine_version": "", "snapshot": "{}",
                      "version": 1, "updated_at": 1.0, "deadlines": []});
    let cases: Vec<(&str, Value)> = vec![
        ("snapshot not text", json!({"snapshot": 5, "version": 1})),
        ("version zero", json!({"snapshot": "{}", "version": 0})),
        ("version bool", json!({"snapshot": "{}", "version": true})),
        ("version missing", json!({"snapshot": "{}"})),
        ("machine_version", json!({"snapshot": "{}", "version": 1, "machine_version": 3})),
        ("updated_at", json!({"snapshot": "{}", "version": 1, "updated_at": "now"})),
        ("deadlines not list", json!({"snapshot": "{}", "version": 1, "deadlines": 7})),
        (
            "deadline shape",
            json!({"snapshot": "{}", "version": 1, "deadlines": [{"state_id": "a"}]}),
        ),
        ("not an object", json!([1, 2])),
    ];
    for (label, value) in cases {
        write_raw(dir.path(), "k.xsm.json", &value);
        assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))), "{label}");
    }
    fs::write(dir.path().join("k.xsm.json"), b"\xff\xfe not utf8").unwrap();
    assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))));
    fs::write(dir.path().join("k.xsm.json"), "{\"snapshot\": ").unwrap();
    assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))));
    write_raw(dir.path(), "k.xsm.json", &good);
    assert!(s.load("k").unwrap().is_some());
}

#[test]
fn oversized_record_is_refused_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let big = quick(dir.path());
    put(&big, "k", &"x".repeat(100));
    let small =
        FileStore::open_with(dir.path(), FileStoreOptions::default().max_snapshot_bytes(10))
            .unwrap();
    assert!(matches!(
        small.load("k"),
        Err(StoreError::SnapshotTooLarge { size: 100, limit: 10, .. })
    ));
    // a file far beyond any legitimate record is refused before parsing
    fs::write(dir.path().join("huge.xsm.json"), vec![b' '; 7 * 1024 * 1024]).unwrap();
    assert!(matches!(small.load("huge"), Err(StoreError::SnapshotTooLarge { .. })));
}

#[test]
fn stores_on_one_directory_see_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (quick(dir.path()), quick(dir.path()));
    assert_eq!(put(&a, "k", "from-a"), 1);
    assert_eq!(b.load("k").unwrap().unwrap().snapshot, "from-a");
    assert_eq!(put(&b, "k", "from-b"), 2);
    assert_eq!(a.load("k").unwrap().unwrap().version, 2);
}

#[test]
fn separate_instances_race_through_the_os_lock() {
    let dir = tempfile::tempdir().unwrap();
    let wins = AtomicUsize::new(0);
    thread::scope(|scope| {
        for i in 0..6 {
            let wins = &wins;
            let path = dir.path();
            scope.spawn(move || {
                let store = quick(path); // its own in-process gate: only the OS lock arbitrates
                let opts = SaveOptions { expected_version: Some(0), ..SaveOptions::default() };
                match store.save("race", &format!("w{i}"), &opts) {
                    Ok(1) => {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(StoreError::Conflict { .. }) => {}
                    other => panic!("unexpected {other:?}"),
                }
            });
        }
    });
    assert_eq!(wins.load(Ordering::SeqCst), 1);
}

#[test]
fn lock_is_held_across_instances() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (quick(dir.path()), quick(dir.path()));
    let guard = a.lock("k", Duration::from_secs(1)).unwrap();
    let err = b.lock("k", Duration::from_millis(100)).unwrap_err();
    match err {
        StoreError::LockTimeout { holder, .. } => {
            let holder = holder.expect("the lock file records its holder");
            assert!(holder.starts_with(&std::process::id().to_string()), "{holder}");
        }
        other => panic!("expected LockTimeout, got {other:?}"),
    }
    drop(guard);
    b.lock("k", Duration::from_secs(1)).unwrap();
}

#[test]
fn forget_removes_record_and_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    put(&s, "k", "{}");
    let report = s.forget("k").unwrap();
    assert_eq!(report.snapshots, 1);
    assert_eq!(report.other, [("locks".to_owned(), 1)]);
    assert_eq!(names(dir.path()), Vec::<String>::new());
}

#[test]
fn health_reports_directory() {
    let dir = tempfile::tempdir().unwrap();
    let s = quick(dir.path());
    let h = s.health().unwrap();
    assert!(h.ok);
    assert_eq!(h.details[0].0, "directory");
    assert_eq!(s.directory(), dir.path());
    fs::remove_dir_all(dir.path()).unwrap();
    assert!(!s.health().unwrap().ok, "a vanished directory is unhealthy");
}

#[cfg(unix)]
#[test]
fn unix_permissions_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let s = quick(&root);
    put(&s, "k", "{}");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("k.xsm.json")), 0o600);
    assert_eq!(mode(&root.join("k.lock")), 0o600);
}

// ------------------------------------------------------------------------------------
// SqliteStore
// ------------------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
mod sqlite {
    use rusqlite::Connection;
    use xstate_persistence::{JournalMode, SqliteStore, SqliteStoreOptions};

    use super::*;

    fn raw(path: &Path) -> Connection {
        Connection::open(path).unwrap()
    }

    #[test]
    fn schema_matches_python() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        drop(SqliteStore::open(&db).unwrap());
        let c = raw(&db);
        let version: i64 = c.query_row("SELECT version FROM xsm_schema", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let sql = |name: &str| -> String {
            c.query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [name], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            sql("statecharts"),
            "CREATE TABLE statecharts (\n        key             TEXT PRIMARY KEY,\n        snapshot        TEXT NOT NULL,\n        version         INTEGER NOT NULL,\n        machine_version TEXT NOT NULL DEFAULT '',\n        updated_at      REAL NOT NULL\n    )"
        );
        assert_eq!(
            sql("deadlines"),
            "CREATE TABLE deadlines (\n        key         TEXT NOT NULL,\n        state_id    TEXT NOT NULL,\n        entry_seq   INTEGER NOT NULL,\n        due_at_wall REAL NOT NULL,\n        delay_ms    INTEGER NOT NULL,\n        event_type  TEXT NOT NULL,\n        FOREIGN KEY (key) REFERENCES statecharts(key) ON DELETE CASCADE\n    )"
        );
        assert_eq!(sql("deadlines_due"), "CREATE INDEX deadlines_due ON deadlines(due_at_wall)");
        assert_eq!(sql("deadlines_key"), "CREATE INDEX deadlines_key ON deadlines(key)");
    }

    #[test]
    fn v1_database_is_upgraded_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("v1.db");
        raw(&db)
            .execute_batch(
                "CREATE TABLE xsm_schema (version INTEGER NOT NULL);
                 INSERT INTO xsm_schema(version) VALUES (1);
                 CREATE TABLE statecharts (key TEXT PRIMARY KEY, snapshot TEXT NOT NULL,
                    version INTEGER NOT NULL, machine_version TEXT NOT NULL DEFAULT '',
                    updated_at REAL NOT NULL);
                 CREATE TABLE deadlines (key TEXT NOT NULL, state_id TEXT NOT NULL,
                    entry_seq INTEGER NOT NULL, due_at_wall REAL NOT NULL, delay_ms INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    FOREIGN KEY (key) REFERENCES statecharts(key) ON DELETE CASCADE);
                 INSERT INTO statecharts VALUES ('old', '{}', 7, 'm', 1.0);
                 DROP INDEX IF EXISTS deadlines_key;",
            )
            .unwrap();
        let s = SqliteStore::open(&db).unwrap();
        assert_eq!(s.load("old").unwrap().unwrap().version, 7, "existing rows survive");
        let c = raw(&db);
        let version: i64 = c.query_row("SELECT version FROM xsm_schema", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let index: i64 = c
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'deadlines_key'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn newer_schema_is_refused_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        drop(SqliteStore::open(&db).unwrap());
        raw(&db).execute("UPDATE xsm_schema SET version = 3", []).unwrap();
        let err = SqliteStore::open(&db).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(ref m) if m.contains("newer")), "{err:?}");
    }

    #[test]
    fn foreign_statecharts_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        raw(&db).execute_batch("CREATE TABLE statecharts (id INTEGER)").unwrap();
        assert!(SqliteStore::open(&db).is_err());
    }

    #[test]
    fn non_database_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("junk.db");
        fs::write(&db, "this is not a database, just some long enough text to fill a header....")
            .unwrap();
        assert!(matches!(SqliteStore::open(&db), Err(StoreError::Unavailable(_))));
    }

    #[test]
    fn data_survives_reopen_and_health_reports_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        {
            let s = SqliteStore::open(&db).unwrap();
            let deadlines = [dl("m.a", 1, 10.0, 5)];
            let opts = SaveOptions {
                machine_version: "9",
                deadlines: &deadlines,
                ..SaveOptions::default()
            };
            s.save("k", "{\"x\":1}", &opts).unwrap();
        }
        let s = SqliteStore::open(&db).unwrap();
        let rec = s.load("k").unwrap().unwrap();
        assert_eq!((rec.version, rec.machine_version.as_str()), (1, "9"));
        assert_eq!(rec.deadlines.len(), 1);
        let h = s.health().unwrap();
        assert!(h.ok);
        let detail = |k: &str| h.details.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(detail("schema_version").as_deref(), Some("2"));
        assert_eq!(detail("path").as_deref(), Some(db.to_str().unwrap()));
        assert_eq!(detail("journal_mode").as_deref(), Some("wal"));
        assert_eq!(detail("keys").as_deref(), Some("1"));
        assert_eq!(s.path(), db.to_str().unwrap());
    }

    #[test]
    fn journal_mode_option_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let options = SqliteStoreOptions::default().journal_mode(JournalMode::Delete);
        let s = SqliteStore::open_with(dir.path().join("d.db"), options).unwrap();
        let h = s.health().unwrap();
        assert!(h.details.contains(&("journal_mode".to_owned(), "delete".to_owned())));
    }

    #[test]
    fn delete_cascades_and_save_replaces_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let s = SqliteStore::open(&db).unwrap();
        let count = || -> i64 {
            raw(&db).query_row("SELECT count(*) FROM deadlines", [], |r| r.get(0)).unwrap()
        };
        let three = [dl("a", 1, 1.0, 1), dl("b", 2, 2.0, 1), dl("c", 3, 3.0, 1)];
        s.save("k", "{}", &SaveOptions { deadlines: &three, ..SaveOptions::default() }).unwrap();
        assert_eq!(count(), 3);
        s.save("k", "{}", &SaveOptions { deadlines: &three[..1], ..SaveOptions::default() })
            .unwrap();
        assert_eq!(count(), 1);
        assert!(s.delete("k").unwrap());
        assert_eq!(count(), 0, "ON DELETE CASCADE");
    }

    #[test]
    fn forget_also_erases_log_rows_sharing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let s = SqliteStore::open(&db).unwrap();
        put(&s, "k", "{}");
        let before = s.forget("k").unwrap();
        assert_eq!(
            before.other,
            [("log_entries".to_owned(), 0)],
            "no transitions table: zero rows"
        );
        put(&s, "k", "{}");
        raw(&db)
            .execute_batch(
                "CREATE TABLE transitions (machine_id TEXT, seq INTEGER);
                 INSERT INTO transitions VALUES ('k', 1), ('k', 2), ('other', 1);",
            )
            .unwrap();
        let report = s.forget("k").unwrap();
        assert_eq!((report.snapshots, report.deadlines), (1, 0));
        assert_eq!(report.other, [("log_entries".to_owned(), 2)]);
        let left: i64 =
            raw(&db).query_row("SELECT count(*) FROM transitions", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn damaged_rows_are_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let s = SqliteStore::open(&db).unwrap();
        put(&s, "k", "{}");
        let c = raw(&db);
        c.execute("UPDATE statecharts SET snapshot = x'ff00'", []).unwrap();
        assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))), "BLOB snapshot");
        c.execute("UPDATE statecharts SET snapshot = '{}', version = 'x'", []).unwrap();
        assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))), "text version");
        c.execute("UPDATE statecharts SET version = 1", []).unwrap();
        c.execute("INSERT INTO deadlines VALUES ('k', 'a', 1, 2.0, -5, 'after.x.a')", []).unwrap();
        assert!(matches!(s.load("k"), Err(StoreError::Corrupt(_))), "negative delay");
    }

    #[test]
    fn oversized_row_is_refused_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        put(&SqliteStore::open(&db).unwrap(), "k", &"x".repeat(100));
        let small =
            SqliteStore::open_with(&db, SqliteStoreOptions::default().max_snapshot_bytes(10))
                .unwrap();
        assert!(matches!(
            small.load("k"),
            Err(StoreError::SnapshotTooLarge { size: 100, limit: 10, .. })
        ));
    }

    #[test]
    fn two_stores_on_one_file_share_versions() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let (a, b) = (SqliteStore::open(&db).unwrap(), SqliteStore::open(&db).unwrap());
        assert_eq!(put(&a, "k", "a"), 1);
        assert_eq!(put(&b, "k", "b"), 2);
        let err =
            a.save("k", "c", &SaveOptions { expected_version: Some(1), ..SaveOptions::default() });
        assert!(matches!(err, Err(StoreError::Conflict { actual: Some(2), .. })));
    }

    #[test]
    fn lock_is_database_wide_and_blocks_other_writers() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let options = SqliteStoreOptions::default().busy_timeout(Duration::from_millis(200));
        let s = SqliteStore::open_with(&db, options).unwrap();
        let guard = s.lock("a", Duration::from_secs(1)).unwrap();
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    // a different key still waits: SQLite has no row locks
                    assert!(matches!(
                        s.lock("b", Duration::from_millis(100)),
                        Err(StoreError::LockTimeout { .. })
                    ));
                    // another thread's write waits for the lock holder and times out
                    assert!(matches!(
                        s.save("b", "{}", &SaveOptions::default()),
                        Err(StoreError::LockTimeout { .. })
                    ));
                    // readers are not blocked in WAL mode
                    assert!(s.load("b").unwrap().is_none());
                })
                .join()
                .unwrap();
        });
        drop(guard);
        assert_eq!(put(&s, "b", "{}"), 1);
    }

    #[test]
    fn writes_inside_a_lock_are_atomic_with_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let s = SqliteStore::open(&db).unwrap();
        let other = SqliteStore::open(&db).unwrap(); // opening needs the write lock: do it first
        let guard = s.lock("k", Duration::from_secs(1)).unwrap();
        put(&s, "k", "inside");
        // an outside reader does not see the uncommitted write yet
        assert!(other.load("k").unwrap().is_none());
        // a failing write inside the lock rolls back to its savepoint only
        assert!(
            s.save("k", "x", &SaveOptions { expected_version: Some(9), ..SaveOptions::default() })
                .is_err()
        );
        drop(guard);
        assert_eq!(other.load("k").unwrap().unwrap().snapshot, "inside");
    }

    #[test]
    fn reentrant_lock_on_the_same_thread_joins() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStore::open(dir.path().join("x.db")).unwrap();
        let outer = s.lock("k", Duration::from_secs(1)).unwrap();
        let inner = s.lock("k", Duration::from_millis(50)).unwrap();
        drop(inner);
        put(&s, "k", "{}");
        drop(outer);
        assert_eq!(s.load("k").unwrap().unwrap().version, 1);
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        drop(SqliteStore::open(&db).unwrap());
        assert_eq!(fs::metadata(&db).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
