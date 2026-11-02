//! Integration tests of the PostgreSQL backends (`PostgresStore`, `PostgresInbox`,
//! `PostgresLog`).
//!
//! They talk to the server at `XSM_POSTGRES_URL` (default `postgres://xsm:xsm@127.0.0.1:5432/xsm`),
//! create one throw-away schema per test (every connection of the store under test has it as its
//! `search_path`), drop it afterwards, and skip with a printed note when PostgreSQL is not
//! reachable. The inbox and log tests live in `postgres/inbox_log.rs`; the tests that share a
//! database with the Python library live in `postgres/cross_language.rs`.
#![cfg(feature = "postgres")]
// The skip notes are printed on purpose: a CI without PostgreSQL must say why nothing ran; `expect`
// states a precondition in the helpers of this integration-test crate (clippy.toml only exempts
// `#[test]` functions).
#![allow(clippy::print_stderr, clippy::expect_used)]

mod common;

use std::fmt::Display;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{count_of, counter_logic, counter_machine, version_of};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::runtime::{Builder, Runtime};
use xstate_core::snapshot::Deadline;
use xstate_persistence::idempotency::InboxStore;
use xstate_persistence::locking::{PersistedOptions, PessimisticLock, persisted};
use xstate_persistence::log::TransitionLogStore;
use xstate_persistence::postgres_backend::SCHEMA_VERSION;
use xstate_persistence::{
    PostgresInbox, PostgresLog, PostgresStore, PostgresStoreOptions, SaveOptions, StateStore,
    StoreError,
};

const DEFAULT_URL: &str = "postgres://xsm:xsm@127.0.0.1:5432/xsm";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn postgres_url() -> String {
    std::env::var("XSM_POSTGRES_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

/// A raw sqlx connection (on a runtime of its own) for inspecting and damaging the layout.
struct Raw {
    rt: Runtime,
    pool: PgPool,
}

impl Raw {
    /// `None` when the server is unreachable.
    fn connect(url: &str) -> Option<Self> {
        let rt = Builder::new_current_thread().enable_all().build().ok()?;
        let options: PgConnectOptions = url.parse().ok()?;
        let pool = rt
            .block_on(
                PgPoolOptions::new()
                    .max_connections(2)
                    .acquire_timeout(Duration::from_secs(2))
                    .connect_with(options),
            )
            .ok()?;
        Some(Self { rt, pool })
    }

    fn exec(&self, sql: &str) -> u64 {
        let query = sqlx::query(AssertSqlSafe(sql.to_owned()));
        ok(self.rt.block_on(query.execute(&self.pool))).rows_affected()
    }

    /// The first column of the first row, as text.
    fn text(&self, sql: &str) -> String {
        let query = sqlx::query_scalar::<_, String>(AssertSqlSafe(sql.to_owned()));
        ok(self.rt.block_on(query.fetch_one(&self.pool)))
    }

    fn int(&self, sql: &str) -> i64 {
        let query = sqlx::query_scalar::<_, i64>(AssertSqlSafe(sql.to_owned()));
        ok(self.rt.block_on(query.fetch_one(&self.pool)))
    }
}

/// One test's private schema: created on construction, dropped (with everything in it) on drop.
struct Ns {
    url: String,
    schema: String,
    raw: Raw,
}

impl Ns {
    fn new(test: &str) -> Option<Self> {
        let url = postgres_url();
        let Some(raw) = Raw::connect(&url) else {
            common::service_unavailable(
                test,
                &format!("no PostgreSQL reachable (XSM_POSTGRES_URL, default {DEFAULT_URL})"),
            );
            return None;
        };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let schema = format!("xsmtest_{test}_{}_{nanos}", std::process::id());
        raw.exec(&format!("CREATE SCHEMA {schema}"));
        Some(Self { url, schema, raw })
    }

    fn options(&self) -> PostgresStoreOptions {
        PostgresStoreOptions::default().schema(self.schema.clone())
    }

    fn store(&self) -> PostgresStore {
        ok(PostgresStore::connect(&self.url, &self.options()))
    }

    /// `table` qualified with this test's schema, for raw SQL.
    fn t(&self, table: &str) -> String {
        format!("{}.{table}", self.schema)
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        let drop_schema = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema);
        // Best effort: a failure here must not mask the test's own verdict.
        let _ =
            self.raw.rt.block_on(sqlx::query(AssertSqlSafe(drop_schema)).execute(&self.raw.pool));
    }
}

macro_rules! ns {
    ($name:expr) => {
        match Ns::new($name) {
            Some(ns) => ns,
            None => return,
        }
    };
}
#[path = "postgres/cross_language.rs"]
mod cross_language;
#[path = "postgres/inbox_log.rs"]
mod inbox_log;

fn deadline(state: &str, entry_seq: u64, due: f64, delay: i64) -> Deadline {
    Deadline {
        state_id: state.to_owned(),
        entry_seq,
        due_at_wall: due,
        delay_ms: json!(delay),
        event_type: format!("after.{delay}.{state}"),
    }
}

fn with_deadlines(deadlines: &[Deadline]) -> SaveOptions<'_> {
    SaveOptions { deadlines, machine_version: "1.0", ..SaveOptions::default() }
}

fn expecting(version: u64) -> SaveOptions<'static> {
    SaveOptions { expected_version: Some(version), ..SaveOptions::default() }
}

fn conflict(result: Result<u64, StoreError>) -> (u64, Option<u64>) {
    match result {
        Err(StoreError::Conflict { expected, actual, .. }) => (expected, actual),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------- the store

#[test]
fn save_load_versions_and_conflicts() {
    let ns = ns!("versions");
    let store = ns.store();
    assert_eq!(ok(store.save("k", r#"{"a":1}"#, &SaveOptions::default())), 1);
    let rec = ok(store.load("k")).expect("saved");
    assert_eq!(
        (rec.version, rec.snapshot.as_str(), rec.machine_version.as_str()),
        (1, r#"{"a":1}"#, "")
    );
    assert!(rec.updated_at > 1.6e9 && rec.deadlines.is_empty());

    assert_eq!(ok(store.save("k", "{}", &expecting(1))), 2);
    assert_eq!(conflict(store.save("k", "{}", &expecting(1))), (1, Some(2)));
    assert_eq!(conflict(store.save("k", "{}", &expecting(0))), (0, Some(2)));
    assert_eq!(conflict(store.save("absent", "{}", &expecting(3))), (3, None));
    assert_eq!(conflict(store.save("absent", "{}", &expecting(u64::MAX))), (u64::MAX, None));
    assert_eq!(ok(store.save("absent", "{}", &expecting(0))), 1);
    assert_eq!(ok(store.save("k", "{}", &SaveOptions::default())), 3);
    assert_eq!(ok(store.load("k")).expect("saved").snapshot, "{}", "a conflict wrote nothing");

    assert!(ok(store.delete("k")));
    assert!(ok(store.load("k")).is_none() && !ok(store.delete("k")));
    assert_eq!(ok(store.save("k", "{}", &SaveOptions::default())), 1, "delete restarts at 1");
}

#[test]
fn non_ascii_keys_and_snapshots_round_trip() {
    let ns = ns!("unicode");
    let store = ns.store();
    let (key, snap) = ("Ordér:42 日本 😀", r#"{"note":"héllo 日本 😀"}"#);
    ok(store.save(key, snap, &SaveOptions::default()));
    assert_eq!(ok(store.load(key)).expect("saved").snapshot, snap);
    assert_eq!(ok(store.list_keys("Ordér", 10)), [key]);
}

#[test]
fn argument_policy_matches_the_other_stores() {
    let ns = ns!("policy");
    let store = ok(PostgresStore::connect(&ns.url, &ns.options().max_snapshot_bytes(10)));
    assert!(matches!(
        store.save("", "{}", &SaveOptions::default()),
        Err(StoreError::InvalidKey(_))
    ));
    assert!(matches!(store.load("a\0b"), Err(StoreError::InvalidKey(_))));
    assert!(matches!(
        store.save(&"x".repeat(201), "{}", &SaveOptions::default()),
        Err(StoreError::InvalidKey(_))
    ));
    assert!(matches!(
        store.save("k", "01234567890", &SaveOptions::default()),
        Err(StoreError::SnapshotTooLarge { size: 11, limit: 10, .. })
    ));
    let long = SaveOptions { machine_version: &"v".repeat(256), ..SaveOptions::default() };
    assert!(store.save("k", "{}", &long).is_err());
    let nul = SaveOptions { machine_version: "a\0b", ..SaveOptions::default() };
    assert!(store.save("k", "{}", &nul).is_err());
    assert!(
        matches!(
            store.save("k", "{\"a\":\"\0\"}", &SaveOptions::default()),
            Err(StoreError::InvalidKey(_))
        ),
        "PostgreSQL cannot store NUL"
    );
    // A record another writer planted over the cap is refused on load.
    ns.raw.exec(&format!(
        "INSERT INTO {} VALUES ('big', '{}', 1, '', 1.0)",
        ns.t("xsm_snapshots"),
        "x".repeat(11)
    ));
    assert!(matches!(store.load("big"), Err(StoreError::SnapshotTooLarge { .. })));
}

#[test]
fn the_layout_is_what_python_writes() {
    let ns = ns!("layout");
    let store = ns.store();
    let dls = [deadline("m.a", 3, 1_700_000_000.5, 1500)];
    ok(store.save("order:1", r#"{"v":4}"#, &with_deadlines(&dls)));
    ok(store.save("order:1", r#"{"v":4}"#, &with_deadlines(&dls)));

    let snap = format!(
        "SELECT concat_ws('|', key, snapshot, version, machine_version) FROM {}",
        ns.t("xsm_snapshots")
    );
    assert_eq!(ns.raw.text(&snap), r#"order:1|{"v":4}|2|1.0"#);
    let updated = format!("SELECT updated_at::text FROM {}", ns.t("xsm_snapshots"));
    assert!(ns.raw.text(&updated).parse::<f64>().is_ok_and(|t| t > 1.6e9));
    let rows = format!(
        "SELECT concat_ws('|', source, key, state_id, entry_seq, due_at_wall, delay_ms, event_type) FROM {}",
        ns.t("xsm_deadlines")
    );
    assert_eq!(ns.raw.text(&rows), "xsm_snapshots|order:1|m.a|3|1700000000.5|1500|after.1500.m.a");
    let version = format!("SELECT concat_ws('|', component, version) FROM {}", ns.t("xsm_schema"));
    assert_eq!(ns.raw.text(&version), format!("sqlalchemy|{SCHEMA_VERSION}"));
    assert_eq!(ok(store.load("order:1")).expect("saved").deadlines, dls);
}

#[test]
fn list_keys_filters_by_literal_prefix_and_sorts_by_code_point() {
    let ns = ns!("list");
    let store = ns.store();
    for key in ["order:2", "order:1", "other", "a%b", "a_b", "ab", "B", "a"] {
        ok(store.save(key, "{}", &SaveOptions::default()));
    }
    assert_eq!(ok(store.list_keys("order:", 10)), ["order:1", "order:2"]);
    assert_eq!(ok(store.list_keys("a%", 10)), ["a%b"], "LIKE characters match literally");
    assert_eq!(ok(store.list_keys("a_", 10)), ["a_b"]);
    assert_eq!(ok(store.list_keys("", 3)), ["B", "a", "a%b"], "upper case sorts before lower");
    assert_eq!(ok(store.list_keys("", 0)), Vec::<String>::new());
    assert_eq!(ok(store.list_keys("a\0", 10)), Vec::<String>::new());
    // A key `load()` would refuse is never advertised.
    ns.raw.exec(&format!("INSERT INTO {} VALUES ('', '{{}}', 1, '', 1.0)", ns.t("xsm_snapshots")));
    assert!(!ok(store.list_keys("", 100)).contains(&String::new()));
}

#[test]
fn due_keys_reads_the_deadline_index() {
    let ns = ns!("due");
    let store = ns.store();
    ok(store.save("a", "{}", &with_deadlines(&[deadline("m.a", 1, 100.0, 1)])));
    ok(store.save(
        "b",
        "{}",
        &with_deadlines(&[deadline("m.x", 1, 200.0, 1), deadline("m.y", 2, 50.0, 1)]),
    ));
    ok(store.save("c", "{}", &with_deadlines(&[deadline("m.a", 1, 300.0, 1)])));
    ok(store.save("none", "{}", &SaveOptions::default()));

    assert_eq!(ok(store.due_keys(150.0, 10)), ["b", "a"], "soonest first, one entry per key");
    assert_eq!(ok(store.due_keys(1000.0, 10)), ["b", "a", "c"]);
    assert_eq!(ok(store.due_keys(1000.0, 1)), ["b"]);
    assert!(ok(store.due_keys(10.0, 10)).is_empty() && ok(store.due_keys(1000.0, 0)).is_empty());

    // Saving again replaces the key's deadlines; deleting drops them.
    ok(store.save("b", "{}", &SaveOptions::default()));
    assert_eq!(ok(store.due_keys(150.0, 10)), ["a"]);
    ok(store.delete("a"));
    assert_eq!(ok(store.due_keys(150.0, 10)), Vec::<String>::new());
    let remaining = format!("SELECT count(*) FROM {}", ns.t("xsm_deadlines"));
    assert_eq!(ns.raw.int(&remaining), 1, "only c's");
}

#[test]
fn two_snapshot_tables_share_a_schema_without_seeing_each_other() {
    let ns = ns!("tables");
    let a = ns.store();
    let b = ok(PostgresStore::connect(&ns.url, &ns.options().table("orders_state")));
    assert_eq!(b.table(), "orders_state");
    ok(a.save("k", "{}", &with_deadlines(&[deadline("m.a", 1, 10.0, 1)])));
    ok(b.save("k", r#"{"b":1}"#, &with_deadlines(&[deadline("m.b", 1, 20.0, 1)])));
    assert_eq!(ok(a.load("k")).expect("a").snapshot, "{}");
    assert_eq!(ok(b.load("k")).expect("b").snapshot, r#"{"b":1}"#);
    assert_eq!(ok(a.load("k")).expect("a").deadlines[0].state_id, "m.a");
    assert_eq!(ok(b.load("k")).expect("b").deadlines[0].state_id, "m.b");
    assert_eq!(ok(b.due_keys(100.0, 10)), ["k"]);
    ok(b.forget("k"));
    assert!(ok(b.load("k")).is_none() && ok(a.load("k")).is_some());
    assert_eq!(ok(a.due_keys(100.0, 10)), ["k"], "a's deadline survived b's forget");
}

#[test]
fn damaged_records_are_corrupt_not_panics() {
    let ns = ns!("corrupt");
    let store = ns.store();
    let snapshots = ns.t("xsm_snapshots");
    ns.raw.exec(&format!("INSERT INTO {snapshots} VALUES ('zero', '{{}}', 0, '', 1.0)"));
    assert!(matches!(store.load("zero"), Err(StoreError::Corrupt(_))));
    ns.raw.exec(&format!("INSERT INTO {snapshots} VALUES ('d', '{{}}', 1, '', 1.0)"));
    ns.raw.exec(&format!(
        "INSERT INTO {} (source, key, state_id, entry_seq, due_at_wall, delay_ms, event_type) \
         VALUES ('xsm_snapshots', 'd', 'm.a', -1, 1.0, 1, 'e')",
        ns.t("xsm_deadlines")
    ));
    assert!(matches!(store.load("d"), Err(StoreError::Corrupt(_))));
}

// ------------------------------------------------------------------------------------ locks

#[test]
fn locks_time_out_release_and_only_the_owner_releases() {
    let ns = ns!("lock");
    let store = ns.store();
    let guard = ok(store.lock("k", Duration::from_secs(1)));
    assert_eq!(guard.key(), "k");
    let row =
        format!("SELECT concat_ws('|', source, key, length(owner)) FROM {}", ns.t("xsm_locks"));
    assert_eq!(ns.raw.text(&row), "xsm_snapshots|k|32");

    match store.lock("k", Duration::from_millis(60)) {
        Err(StoreError::LockTimeout { key, .. }) => assert_eq!(key, "k"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    drop(ok(store.lock("other", Duration::ZERO)));
    drop(guard);
    let count = format!("SELECT count(*) FROM {}", ns.t("xsm_locks"));
    assert_eq!(ns.raw.int(&count), 0, "the guard deleted its row");
    drop(ok(store.lock("k", Duration::ZERO)));

    // Only the owner releases: a guard whose lease was taken over must not free the new owner's.
    let stale = ok(store.lock("k", Duration::ZERO));
    ns.raw.exec(&format!("UPDATE {} SET owner = 'someone-else'", ns.t("xsm_locks")));
    drop(stale);
    assert_eq!(ns.raw.int(&count), 1);
}

#[test]
fn a_lease_expires_after_lock_ttl_and_is_reclaimed() {
    let ns = ns!("lockttl");
    let store =
        ok(PostgresStore::connect(&ns.url, &ns.options().lock_ttl(Duration::from_millis(600))));
    let first = ok(store.lock("k", Duration::ZERO));
    thread::sleep(Duration::from_millis(900));
    let second = ok(store.lock("k", Duration::ZERO));
    drop(first);
    let owners = format!("SELECT count(*) FROM {}", ns.t("xsm_locks"));
    assert_eq!(ns.raw.int(&owners), 1, "the expired holder's drop left the new lease alone");
    assert!(matches!(
        store.lock("k", Duration::from_millis(30)),
        Err(StoreError::LockTimeout { .. })
    ));
    drop(second);
    assert_eq!(ns.raw.int(&owners), 0);
}

#[test]
fn a_waiting_lock_is_granted_when_the_holder_lets_go() {
    let ns = ns!("lockwait");
    let store = Arc::new(ns.store());
    let guard = ok(store.lock("k", Duration::ZERO));
    let waiter = {
        let store = Arc::clone(&store);
        thread::spawn(move || store.lock("k", Duration::from_secs(10)).map(|g| g.key().to_owned()))
    };
    thread::sleep(Duration::from_millis(100));
    drop(guard);
    assert_eq!(ok(waiter.join().unwrap_or_else(|_| panic!("waiter panicked"))), "k");
}

#[test]
fn forget_erases_the_record_deadlines_lease_and_log_but_not_inbox_rows() {
    let ns = ns!("forget");
    let store = ns.store();
    let (log, inbox) = (PostgresLog::new(&store), PostgresInbox::new(&store));
    ok(store.save(
        "k",
        "{}",
        &with_deadlines(&[deadline("m.a", 1, 10.0, 1), deadline("m.b", 2, 20.0, 1)]),
    ));
    ok(store.save("other", "{}", &with_deadlines(&[deadline("m.a", 1, 10.0, 1)])));
    let _held = ok(store.lock("k", Duration::ZERO));
    for seq in 1..=3 {
        ok(log.append(&inbox_log::record("k", seq, 1.0)));
    }
    ok(log.append(&inbox_log::record("other", 1, 1.0)));
    ok(inbox.claim("k", "idem", "fp", None));

    let report = ok(store.forget("k"));
    assert_eq!((report.snapshots, report.deadlines), (1, 2));
    assert_eq!(report.other, [("locks".to_owned(), 1), ("log_entries".to_owned(), 3)]);
    assert!(ok(store.load("k")).is_none());
    assert_eq!(ok(store.due_keys(100.0, 10)), ["other"]);
    let count = |t: &str| ns.raw.int(&format!("SELECT count(*) FROM {}", ns.t(t)));
    assert_eq!((count("xsm_transitions"), count("xsm_inbox"), count("xsm_locks")), (1, 1, 0));
    assert_eq!(ok(store.forget("k")).snapshots, 0, "forgetting twice is fine");
}

// ------------------------------------------------------------------------------ concurrency

#[test]
fn concurrent_creators_have_exactly_one_winner_and_upserts_never_repeat_a_version() {
    let ns = ns!("race");
    let store = Arc::new(ns.store());
    let creators: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.save("k", "{}", &expecting(0)))
        })
        .collect();
    let results: Vec<_> =
        creators.into_iter().map(|t| t.join().unwrap_or_else(|_| panic!("panicked"))).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for loser in results.into_iter().filter(Result::is_err) {
        assert_eq!(conflict(loser), (0, Some(1)));
    }

    let writers: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                (0..5)
                    .map(|_| ok(store.save("k", "{}", &SaveOptions::default())))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut versions: Vec<u64> =
        writers.into_iter().flat_map(|t| t.join().unwrap_or_else(|_| panic!("panicked"))).collect();
    versions.sort_unstable();
    assert_eq!(versions, (2..=21).collect::<Vec<u64>>());
}

#[test]
fn persisted_works_over_postgres_with_a_pessimistic_lock() {
    let ns = ns!("persisted");
    let store = Arc::new(ns.store());
    let machine = counter_machine(None);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            let machine = machine.clone();
            thread::spawn(move || {
                let options = PersistedOptions {
                    lock: Box::new(PessimisticLock { timeout: Duration::from_secs(20) }),
                    ..PersistedOptions::default()
                };
                for _ in 0..5 {
                    ok(persisted(
                        store.as_ref(),
                        "order:1",
                        &machine,
                        &counter_logic(),
                        &options,
                        |i, _| {
                            i.send("INC")?;
                            Ok(())
                        },
                    ));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap_or_else(|_| panic!("worker panicked"));
    }
    assert_eq!(count_of(store.as_ref(), "order:1"), 20);
    assert_eq!(version_of(store.as_ref(), "order:1"), 20);
}

// ----------------------------------------------------------------------- schema and options

#[test]
fn the_schema_version_is_recorded_checked_and_a_newer_one_refused() {
    let ns = ns!("schema");
    drop(ns.store());
    let versions =
        format!("SELECT string_agg(component || ':' || version, ',') FROM {}", ns.t("xsm_schema"));
    assert_eq!(ns.raw.text(&versions), "sqlalchemy:1");
    drop(ns.store()); // idempotent
    assert_eq!(ns.raw.text(&versions), "sqlalchemy:1");

    ns.raw.exec(&format!("UPDATE {} SET version = 2", ns.t("xsm_schema")));
    for create in [true, false] {
        match PostgresStore::connect(&ns.url, &ns.options().create_tables(create)) {
            Err(StoreError::Corrupt(why)) => assert!(why.contains("newer"), "{why}"),
            other => panic!("create_tables={create}: {:?}", other.map(|_| ())),
        }
    }
    ns.raw.exec(&format!("UPDATE {} SET version = 0", ns.t("xsm_schema")));
    assert!(matches!(PostgresStore::connect(&ns.url, &ns.options()), Err(StoreError::Corrupt(_))));
}

#[test]
fn create_tables_false_verifies_instead_of_creating() {
    let ns = ns!("verify");
    let no_create = ns.options().create_tables(false);
    match PostgresStore::connect(&ns.url, &no_create) {
        Err(StoreError::Other(why)) => {
            for table in [
                "xsm_schema",
                "xsm_snapshots",
                "xsm_deadlines",
                "xsm_locks",
                "xsm_inbox",
                "xsm_transitions",
            ] {
                assert!(why.contains(table), "{why}");
            }
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    drop(ns.store());
    assert!(PostgresStore::connect(&ns.url, &no_create).is_ok());
}

#[test]
fn many_processes_opening_a_fresh_database_at_once_all_succeed() {
    let ns = ns!("opening");
    let opens: Vec<_> = (0..8)
        .map(|_| {
            let (url, options) = (ns.url.clone(), ns.options());
            thread::spawn(move || PostgresStore::connect(&url, &options).map(|_| ()))
        })
        .collect();
    for open in opens {
        ok(open.join().unwrap_or_else(|_| panic!("panicked")));
    }
}

#[test]
fn options_and_urls_are_validated() {
    let ns = ns!("options");
    let bad = |options: PostgresStoreOptions| PostgresStore::connect(&ns.url, &options).is_err();
    assert!(bad(ns.options().table("x; DROP TABLE y")));
    assert!(bad(ns.options().table("")));
    assert!(bad(ns.options().table("1abc")));
    assert!(bad(ns.options().table("t".repeat(64))));
    assert!(bad(PostgresStoreOptions::default().schema("a b")));
    assert!(bad(ns.options().lock_ttl(Duration::ZERO)));
    assert!(matches!(
        PostgresStore::connect("not a url", &PostgresStoreOptions::default()),
        Err(StoreError::Other(_))
    ));
    assert!(matches!(
        PostgresStore::connect(
            "postgres://xsm:xsm@127.0.0.1:1/xsm",
            &PostgresStoreOptions::default()
        ),
        Err(StoreError::Unavailable(_))
    ));
}

#[test]
fn health_reports_the_database_and_a_failure_without_leaking_it() {
    let ns = ns!("health");
    let store = ns.store();
    ok(store.save("k", "{}", &SaveOptions::default()));
    let health = ok(store.health());
    assert!(health.ok && health.backend == "postgres");
    let detail =
        |name: &str| health.details.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    assert_eq!(detail("keys").as_deref(), Some("1"));
    assert_eq!(detail("schema_version").as_deref(), Some("1"));
    assert_eq!(detail("table").as_deref(), Some("xsm_snapshots"));

    ns.raw.exec(&format!("DROP TABLE {}", ns.t("xsm_snapshots")));
    let health = ok(store.health());
    assert!(!health.ok);
    let error = health.details.iter().find(|(k, _)| k == "error").map(|(_, v)| v.as_str());
    assert_eq!(error, Some("DatabaseError"), "a kind, not a message: {:?}", health.details);
    assert!(matches!(store.load("k"), Err(StoreError::Unavailable(_))));
}

// ------------------------------------------------------------------------------- from_pool

#[test]
fn from_pool_works_inside_tokio_through_spawn_blocking() {
    let ns = ns!("frompool");
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("{e}"));
    let options: PgConnectOptions = ok(ns.url.parse());
    let options = options.options([("search_path", ns.schema.as_str())]);
    let pool = ok(runtime.block_on(PgPoolOptions::new().max_connections(3).connect_with(options)));
    let probe = pool.clone();
    let handle = runtime.handle().clone();
    let outcome = runtime.block_on(async {
        let created = handle.clone();
        tokio::task::spawn_blocking(move || {
            let store = PostgresStore::from_pool(pool, created, &PostgresStoreOptions::default())?;
            let version = store.save("k", r#"{"from":"pool"}"#, &SaveOptions::default())?;
            let guard = store.lock("k", Duration::from_secs(1))?;
            drop(guard);
            Ok::<_, StoreError>((version, store.load("k")?.map(|r| r.snapshot)))
        })
        .await
    });
    let (version, snapshot) = ok(outcome.unwrap_or_else(|e| panic!("{e}")));
    assert_eq!((version, snapshot.as_deref()), (1, Some(r#"{"from":"pool"}"#)));
    // The caller's pool is still open: the store never closes a pool it did not create.
    assert!(!probe.is_closed());
}

// ------------------------------------------------------------- lease drop, clock, keys, errors

#[test]
fn dropping_a_lock_guard_inside_a_runtime_releases_it_without_panicking() {
    let ns = ns!("lockdropasync");
    let store = ns.store();
    let guard = ok(store.lock("k", Duration::ZERO));
    let rt = Builder::new_current_thread().enable_all().build().expect("a runtime");
    // `Handle::block_on` inside a runtime panics: the release is handed to the store's runtime
    rt.block_on(async move { drop(guard) });
    let again = ok(store.lock("k", Duration::from_secs(5)));
    assert_eq!(again.key(), "k", "the lease was released in the background");
}

#[test]
fn the_lease_expiry_is_measured_on_the_servers_clock() {
    let ns = ns!("lockclock");
    let options = ns.options().lock_ttl(Duration::from_secs(60));
    let store = ok(PostgresStore::connect(&ns.url, &options));
    let _guard = ok(store.lock("k", Duration::ZERO));
    let ahead = ns.raw.int(&format!(
        "SELECT round(expires_at - extract(epoch from clock_timestamp())::float8)::bigint FROM {}",
        ns.t("xsm_locks")
    ));
    assert!((59..=60).contains(&ahead), "the lease ends {ahead} s after the server's now");
}

#[test]
fn list_keys_matches_the_prefix_literally() {
    let ns = ns!("likeprefix");
    let store = ns.store();
    for key in ["100%_a", "100xxa", r"100\b", "100%", "10", "a_b", "axb"] {
        ok(store.save(key, "{}", &SaveOptions::default()));
    }
    // `%`, `_` and `\` are not wildcards or escapes in a prefix
    assert_eq!(ok(store.list_keys("100%_", 10)), ["100%_a"]);
    assert_eq!(ok(store.list_keys(r"100\", 10)), [r"100\b"]);
    assert_eq!(ok(store.list_keys("a_", 10)), ["a_b"]);
    assert_eq!(ok(store.list_keys("100%", 10)), ["100%", "100%_a"]);
    assert_eq!(ok(store.list_keys("", 3)).len(), 3);
}

#[test]
fn driver_errors_never_carry_the_host_or_the_password() {
    let url = "postgres://xsm:sekrit@127.0.0.1:1/xsm";
    let Err(StoreError::Unavailable(message)) =
        PostgresStore::connect(url, &PostgresStoreOptions::default())
    else {
        panic!("an unreachable server is Unavailable")
    };
    assert!(message.starts_with("PostgresStore: "), "{message}");
    for secret in ["127.0.0.1", "sekrit", ":1/"] {
        assert!(!message.contains(secret), "{message}");
    }
}
