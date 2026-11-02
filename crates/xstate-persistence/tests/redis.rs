//! Integration tests of the Redis backends (`RedisStore`, `RedisInbox`, `RedisLog`).
//!
//! They talk to the Redis at `XSM_REDIS_URL` (default `redis://127.0.0.1:6379/`), use a unique
//! key prefix per test, remove only their own keys, and skip with a printed note when Redis is not
//! reachable. The tests that share a Redis with the Python library live in `redis/cross_language.rs`.
#![cfg(feature = "redis")]
// The skip notes are printed on purpose: a CI without Redis must say why nothing ran; `expect`
// states a precondition in the helpers of this integration-test crate (clippy.toml only exempts
// `#[test]` functions).
#![allow(clippy::print_stderr, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{count_of, counter_logic, counter_machine, version_of};
use serde_json::{Map, json};
use xstate_core::snapshot::Deadline;
use xstate_persistence::idempotency::InboxStore;
use xstate_persistence::locking::{PersistedOptions, PessimisticLock, persisted};
use xstate_persistence::log::{Disposition, Origin, TransitionLogStore, TransitionRecord};
use xstate_persistence::{
    RedisInbox, RedisLog, RedisStore, RedisStoreOptions, SaveOptions, StateStore, StoreError,
};

const DEFAULT_URL: &str = "redis://127.0.0.1:6379/";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn redis_url() -> String {
    std::env::var("XSM_REDIS_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

/// A raw connection for inspecting the layout; `None` when Redis is unreachable.
fn raw_connection(url: &str) -> Option<redis::Connection> {
    let client = redis::Client::open(url).ok()?;
    client.get_connection_with_timeout(Duration::from_secs(2)).ok()
}

/// One test's private namespace: a unique prefix, cleaned up on drop.
struct Ns {
    url: String,
    prefix: String,
}

impl Ns {
    fn new(test: &str) -> Option<Self> {
        let url = redis_url();
        if raw_connection(&url).is_none() {
            eprintln!("skipping `{test}`: no Redis at {url} (set XSM_REDIS_URL)");
            return None;
        }
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let prefix = format!("xsmtest-{test}-{}-{nanos}", std::process::id());
        Some(Self { url, prefix })
    }

    fn raw(&self) -> redis::Connection {
        raw_connection(&self.url).unwrap_or_else(|| panic!("Redis went away"))
    }

    fn store(&self) -> RedisStore {
        ok(RedisStore::open(&self.url, &RedisStoreOptions::new(self.prefix.clone())))
    }

    fn inbox(&self) -> RedisInbox {
        ok(RedisInbox::open(&self.url, &self.prefix))
    }

    fn log(&self) -> RedisLog {
        ok(RedisLog::open(&self.url, &self.prefix))
    }

    fn key(&self, tail: &str) -> String {
        format!("{}:{tail}", self.prefix)
    }

    fn hash(&self, name: &str) -> HashMap<String, String> {
        ok(redis::cmd("HGETALL").arg(self.key(name)).query(&mut self.raw()))
    }

    fn command<T: redis::FromRedisValue>(&self, cmd: &mut redis::Cmd) -> T {
        ok(cmd.query(&mut self.raw()))
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        let Some(mut conn) = raw_connection(&self.url) else { return };
        let pattern = format!("{}:*", self.prefix);
        let names: Vec<String> =
            redis::cmd("KEYS").arg(pattern).query(&mut conn).unwrap_or_default();
        for name in names {
            let _: Result<i64, _> = redis::cmd("DEL").arg(name).query(&mut conn);
        }
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

#[path = "redis/cross_language.rs"]
mod cross_language;

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

    let expect = |v| SaveOptions { expected_version: Some(v), ..SaveOptions::default() };
    assert_eq!(ok(store.save("k", "{}", &expect(1))), 2);
    assert_eq!(conflict(store.save("k", "{}", &expect(1))), (1, Some(2)));
    assert_eq!(conflict(store.save("k", "{}", &expect(0))), (0, Some(2)));
    assert_eq!(conflict(store.save("absent", "{}", &expect(3))), (3, None));
    assert_eq!(ok(store.save("absent", "{}", &expect(0))), 1);
    assert_eq!(ok(store.save("k", "{}", &SaveOptions::default())), 3);

    assert!(ok(store.delete("k")));
    assert!(ok(store.load("k")).is_none() && !ok(store.delete("k")));
    assert_eq!(ok(store.save("k", "{}", &SaveOptions::default())), 1, "delete restarts at 1");
}

#[test]
fn argument_policy_matches_the_other_stores() {
    let ns = ns!("policy");
    let store = ok(RedisStore::open(
        &ns.url,
        &RedisStoreOptions::new(ns.prefix.clone()).max_snapshot_bytes(10),
    ));
    assert!(matches!(
        store.save("", "{}", &SaveOptions::default()),
        Err(StoreError::InvalidKey(_))
    ));
    assert!(matches!(store.load("a\0b"), Err(StoreError::InvalidKey(_))));
    assert!(matches!(
        store.save("k", "01234567890", &SaveOptions::default()),
        Err(StoreError::SnapshotTooLarge { size: 11, limit: 10, .. })
    ));
    let long = SaveOptions { machine_version: &"v".repeat(256), ..SaveOptions::default() };
    assert!(store.save("k", "{}", &long).is_err());
    // A record another writer planted over the cap is refused on load.
    ns.command::<i64>(
        redis::cmd("HSET")
            .arg(ns.key("snap:big"))
            .arg("snapshot")
            .arg("x".repeat(11))
            .arg("version")
            .arg(1),
    );
    assert!(matches!(store.load("big"), Err(StoreError::SnapshotTooLarge { .. })));
}

#[test]
fn the_redis_layout_is_what_python_writes() {
    let ns = ns!("layout");
    let store = ns.store();
    let dls = [deadline("m.a", 3, 1_700_000_000.5, 1500)];
    ok(store.save("order:1", r#"{"v":4}"#, &with_deadlines(&dls)));
    ok(store.save("order:1", r#"{"v":4}"#, &with_deadlines(&dls)));

    let snap = ns.hash("snap:order:1");
    assert_eq!(snap["snapshot"], r#"{"v":4}"#);
    assert_eq!((snap["version"].as_str(), snap["machine_version"].as_str()), ("2", "1.0"));
    assert!(snap["updated_at"].parse::<f64>().is_ok_and(|t| t > 1.6e9));
    assert_eq!(ns.command::<String>(redis::cmd("GET").arg(ns.key("schema"))), "2");
    assert_eq!(ns.command::<Vec<String>>(redis::cmd("SMEMBERS").arg(ns.key("keys"))), ["order:1"]);

    let member = r#"["order:1","m.a",3,"after.1500.m.a"]"#;
    let index: Vec<(String, f64)> =
        ns.command(redis::cmd("ZRANGE").arg(ns.key("deadlines")).arg(0).arg(-1).arg("WITHSCORES"));
    assert_eq!(index, [(member.to_owned(), 1_700_000_000.5)]);
    assert_eq!(
        ns.hash("dl:order:1")[member],
        r#"{"state_id": "m.a", "entry_seq": 3, "due_at_wall": 1700000000.5, "delay_ms": 1500, "event_type": "after.1500.m.a"}"#
    );
    let rec = ok(store.load("order:1")).expect("saved");
    assert_eq!(rec.deadlines, dls);
}

#[test]
fn list_keys_filters_by_literal_prefix_and_sorts() {
    let ns = ns!("list");
    let store = ns.store();
    for key in ["order:2", "order:1", "other", "a*b", "a?b", "ab", "a[1]"] {
        ok(store.save(key, "{}", &SaveOptions::default()));
    }
    assert_eq!(ok(store.list_keys("order:", 10)), ["order:1", "order:2"]);
    assert_eq!(ok(store.list_keys("a*", 10)), ["a*b"], "glob characters match literally");
    assert_eq!(ok(store.list_keys("a?", 10)), ["a?b"]);
    assert_eq!(ok(store.list_keys("a[", 10)), ["a[1]"]);
    assert_eq!(ok(store.list_keys("", 3)), ["a*b", "a?b", "a[1]"]);
    assert_eq!(ok(store.list_keys("", 0)), Vec::<String>::new());
    // A key `load()` would refuse is never advertised.
    ns.command::<i64>(redis::cmd("SADD").arg(ns.key("keys")).arg("x".repeat(201)));
    assert_eq!(ok(store.list_keys("x", 10)), Vec::<String>::new());
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
    assert_eq!(ns.command::<i64>(redis::cmd("ZCARD").arg(ns.key("deadlines"))), 1);
}

#[test]
fn a_key_containing_a_pipe_wakes_itself_not_its_prefix() {
    let ns = ns!("pipe");
    let store = ns.store();
    ok(store.save("a|b", "{}", &with_deadlines(&[deadline("m.s", 1, 10.0, 1)])));
    assert_eq!(ok(store.due_keys(100.0, 10)), ["a|b"]);
}

#[test]
fn layout_one_members_are_still_read_and_cleaned() {
    let ns = ns!("layout1");
    let store = ns.store();
    // What a layout-1 writer left: member "{key}|{field}".
    ns.command::<i64>(redis::cmd("ZADD").arg(ns.key("deadlines")).arg(5.0).arg("old|m.s|1|ev"));
    ns.command::<i64>(redis::cmd("ZADD").arg(ns.key("deadlines")).arg(6.0).arg("gone|m.s|1|ev"));
    ok(store.save("old", "{}", &SaveOptions::default()));
    // `old` has a snapshot (kept), `gone` has none (pruned and not reported).
    assert_eq!(ok(store.due_keys(100.0, 10)), ["old"]);
    assert_eq!(ns.command::<i64>(redis::cmd("ZCARD").arg(ns.key("deadlines"))), 1);
}

#[test]
fn ttl_expires_snapshots_and_due_keys_prunes_the_orphans() {
    let ns = ns!("ttl");
    let store =
        ok(RedisStore::open(&ns.url, &RedisStoreOptions::new(ns.prefix.clone()).ttl_s(0.15)));
    ok(store.save("idle", "{}", &with_deadlines(&[deadline("m.s", 1, 10.0, 1)])));
    assert!(ns.command::<i64>(redis::cmd("PTTL").arg(ns.key("snap:idle"))) > 0);
    assert!(ok(store.load("idle")).is_some());
    thread::sleep(Duration::from_millis(400));
    assert!(ok(store.load("idle")).is_none());
    assert_eq!(ns.command::<i64>(redis::cmd("ZCARD").arg(ns.key("deadlines"))), 1, "orphan member");
    assert_eq!(ok(store.due_keys(100.0, 10)), Vec::<String>::new());
    assert_eq!(ns.command::<i64>(redis::cmd("ZCARD").arg(ns.key("deadlines"))), 0, "pruned");
}

#[test]
fn locks_time_out_release_and_only_the_owner_releases() {
    let ns = ns!("lock");
    let store = ns.store();
    let guard = ok(store.lock("k", Duration::from_secs(1)));
    assert_eq!(guard.key(), "k");
    let token: String = ns.command(redis::cmd("GET").arg(ns.key("lock:k")));
    assert!(token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
    assert!(ns.command::<i64>(redis::cmd("PTTL").arg(ns.key("lock:k"))) > 0);

    match store.lock("k", Duration::from_millis(60)) {
        Err(StoreError::LockTimeout { key, .. }) => assert_eq!(key, "k"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    ok(store.lock("other", Duration::ZERO));
    drop(guard);
    assert_eq!(ns.command::<i64>(redis::cmd("EXISTS").arg(ns.key("lock:k"))), 0);
    drop(ok(store.lock("k", Duration::ZERO)));

    // Compare-and-delete: a guard whose lock was taken over must not free the new owner's.
    let stale = ok(store.lock("k", Duration::ZERO));
    ns.command::<()>(redis::cmd("SET").arg(ns.key("lock:k")).arg("someone-else"));
    drop(stale);
    assert_eq!(ns.command::<String>(redis::cmd("GET").arg(ns.key("lock:k"))), "someone-else");
}

#[test]
fn a_lock_expires_after_lock_ttl() {
    let ns = ns!("lockttl");
    let store =
        ok(RedisStore::open(&ns.url, &RedisStoreOptions::new(ns.prefix.clone()).lock_ttl_ms(100)));
    let _held = ok(store.lock("k", Duration::ZERO));
    thread::sleep(Duration::from_millis(250));
    drop(ok(store.lock("k", Duration::ZERO)));
}

#[test]
fn forget_erases_everything_about_a_key_but_inbox_rows() {
    let ns = ns!("forget");
    let store = ns.store();
    let dls = [deadline("m.a", 1, 10.0, 1), deadline("m.b", 2, 20.0, 1)];
    ok(store.save("k", "{}", &with_deadlines(&dls)));
    ok(store.save("keep", "{}", &SaveOptions::default()));
    let log = ns.log();
    ok(log.append_next(record("k", 0, 1.0)));
    let inbox = ns.inbox();
    assert!(ok(inbox.claim("scope", "idem", "fp", None)));
    let _held = ok(store.lock("k", Duration::ZERO));

    let report = ok(store.forget("k"));
    assert_eq!((report.snapshots, report.deadlines), (1, 2));
    assert_eq!(report.other, [("locks".to_owned(), 1), ("log_entries".to_owned(), 1)]);
    assert!(ok(store.load("k")).is_none() && ok(store.load("keep")).is_some());
    assert_eq!(ok(store.list_keys("", 10)), ["keep"]);
    assert_eq!(ok(store.due_keys(100.0, 10)), Vec::<String>::new());
    assert_eq!(ok(log.next_seq("k")), 1);
    assert!(ok(inbox.get("scope", "idem")).is_some(), "inbox rows are erased by scope");
    assert_eq!(ok(store.forget("k")).snapshots, 0);
}

#[test]
fn damaged_records_are_corrupt_not_panics() {
    let ns = ns!("corrupt");
    let store = ns.store();
    ns.command::<i64>(
        redis::cmd("HSET")
            .arg(ns.key("snap:bad"))
            .arg("snapshot")
            .arg("{}")
            .arg("version")
            .arg("x"),
    );
    assert!(matches!(store.load("bad"), Err(StoreError::Corrupt(_))));
    ns.command::<i64>(redis::cmd("HSET").arg(ns.key("snap:nosnap")).arg("version").arg("1"));
    assert!(matches!(store.load("nosnap"), Err(StoreError::Corrupt(_))));
    // An unreadable deadline row is skipped, the record still loads.
    ok(store.save("k", "{}", &with_deadlines(&[deadline("m.a", 1, 10.0, 1)])));
    ns.command::<i64>(redis::cmd("HSET").arg(ns.key("dl:k")).arg("junk").arg("not json"));
    assert_eq!(ok(store.load("k")).expect("record").deadlines.len(), 1);
}

#[test]
fn the_schema_marker_is_checked_and_upgraded() {
    let ns = ns!("schema");
    let open = || RedisStore::open(&ns.url, &RedisStoreOptions::new(ns.prefix.clone()));
    ns.command::<()>(redis::cmd("SET").arg(ns.key("schema")).arg("3"));
    assert!(open().is_err(), "a newer layout is refused");
    ns.command::<()>(redis::cmd("SET").arg(ns.key("schema")).arg("garbage"));
    assert!(open().is_err());
    ns.command::<()>(redis::cmd("SET").arg(ns.key("schema")).arg("1"));
    ok(open());
    assert_eq!(ns.command::<String>(redis::cmd("GET").arg(ns.key("schema"))), "2");
}

#[test]
fn options_and_urls_are_validated() {
    let url = redis_url();
    let bad = |options: RedisStoreOptions| RedisStore::open(&url, &options).is_err();
    assert!(bad(RedisStoreOptions::new("")));
    assert!(bad(RedisStoreOptions::new("has space")));
    assert!(bad(RedisStoreOptions::new("p").ttl_s(0.0001)));
    assert!(bad(RedisStoreOptions::new("p").lock_ttl_ms(0)));
    assert!(RedisStore::open("not a url", &RedisStoreOptions::new("p")).is_err());
}

#[test]
fn an_unreachable_redis_is_unavailable_and_health_reports_it() {
    let refused = RedisStore::open("redis://127.0.0.1:1/", &RedisStoreOptions::new("p"));
    assert!(matches!(refused, Err(StoreError::Unavailable(_))), "{refused:?}");
}

#[test]
fn health_reports_the_namespace() {
    let ns = ns!("health");
    let store = ns.store();
    ok(store.save("k", "{}", &SaveOptions::default()));
    let health = ok(store.health());
    assert!(health.ok && health.backend == "redis");
    let details: HashMap<_, _> = health.details.into_iter().collect();
    assert_eq!(details["keys"], "1");
    assert_eq!(details["schema_version"], "2");
    assert_eq!(details["prefix"], ns.prefix);
}

#[test]
fn persisted_works_over_redis_with_a_pessimistic_lock() {
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

// ------------------------------------------------------------------------------- the inbox

#[test]
fn inbox_claim_mark_duplicate_and_release() {
    let ns = ns!("inbox");
    let inbox = ns.inbox();
    assert!(ok(inbox.get("s", "k")).is_none());
    assert!(ok(inbox.claim("s", "k", "fp1", None)));
    assert!(!ok(inbox.claim("s", "k", "fp2", None)), "a live claim wins");
    let entry = ok(inbox.get("s", "k")).expect("in flight");
    assert_eq!((entry.fingerprint.as_str(), entry.receipt_json.as_deref()), ("fp1", None));

    ok(inbox.mark("s", "k", r#"{"status": 200}"#, None));
    let done = ok(inbox.get("s", "k")).expect("marked");
    assert_eq!(done.fingerprint, "fp1", "mark keeps the claim's fingerprint");
    assert_eq!(done.receipt_json.as_deref(), Some(r#"{"status": 200}"#));
    assert_eq!(done.expires_at, None);

    ok(inbox.release("s", "k"));
    assert!(ok(inbox.get("s", "k")).is_some(), "a marked entry is not released");
    assert!(ok(inbox.claim("s", "other", "fp", None)));
    ok(inbox.release("s", "other"));
    assert!(ok(inbox.get("s", "other")).is_none());
    assert!(ok(inbox.claim("s", "other", "fp", None)), "a released key can be claimed again");
    assert_eq!(ok(inbox.len()), 2);
}

#[test]
fn inbox_layout_is_what_python_writes() {
    let ns = ns!("inboxlayout");
    let inbox = ns.inbox();
    ok(inbox.claim("t/m/i", "key-1", "fp", Some(3600.0)));
    let raw: String = ns.command(redis::cmd("HGET").arg(ns.key("inbox:t/m/i")).arg("key-1"));
    let entry: serde_json::Value = ok(serde_json::from_str(&raw));
    assert_eq!(entry["fingerprint"], "fp");
    assert!(entry["receipt_json"].is_null());
    assert!(entry["expires_at"].as_str().is_some_and(|t| t.parse::<f64>().is_ok()), "{raw}");
    // The TTL-index member is "{len(scope)}:{scope}{key}".
    let score: Option<f64> =
        ns.command(redis::cmd("ZSCORE").arg(ns.key("inbox_exp")).arg("5:t/m/ikey-1"));
    assert!(score.is_some_and(|s| s > 1.6e9));
}

#[test]
fn inbox_ttl_expiry_purge_and_mark_without_ttl() {
    let ns = ns!("inboxttl");
    let inbox = ns.inbox();
    assert!(ok(inbox.claim("s", "short", "fp", Some(0.2))));
    assert!(ok(inbox.claim("s", "forever", "fp", Some(0.2))));
    ok(inbox.mark("s", "forever", "{}", None));
    thread::sleep(Duration::from_millis(400));
    assert!(ok(inbox.get("s", "short")).is_none(), "expired entries are not live");
    assert!(ok(inbox.get("s", "forever")).is_some());
    assert!(ok(inbox.claim("s", "short", "fp-new", Some(60.0))), "an expired key can be claimed");
    assert_eq!(ok(inbox.get("s", "short")).expect("live").fingerprint, "fp-new");

    // `mark(ttl=None)` removed the index member, so a purge never drops the receipt.
    assert_eq!(ok(inbox.purge_expired(Some(f64::MAX / 2.0))), 1, "only the expiring entry");
    assert!(ok(inbox.get("s", "forever")).is_some());
    assert_eq!(ok(inbox.purge_expired(None)), 0);
}

#[test]
fn inbox_purge_handles_awkward_scopes_and_forget_is_exact() {
    let ns = ns!("inboxscope");
    let inbox = ns.inbox();
    for scope in ["a|b", "ключ/日本", "x*", "xy"] {
        assert!(ok(inbox.claim(scope, "k|1", "fp", Some(30.0))));
    }
    assert_eq!(ok(inbox.forget("x*")), 1, "a glob look-alike is untouched");
    assert!(ok(inbox.get("xy", "k|1")).is_some());
    assert_eq!(ok(inbox.purge_expired(Some(f64::MAX / 2.0))), 3);
    assert_eq!(ok(inbox.len()), 0);
    assert!(ok(inbox.is_empty()));
    assert_eq!(ns.command::<i64>(redis::cmd("ZCARD").arg(ns.key("inbox_exp"))), 0);
}

// --------------------------------------------------------------------------------- the log

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn record(machine_id: &str, seq: u64, ts: f64) -> TransitionRecord {
    TransitionRecord {
        machine_id: machine_id.into(),
        seq,
        ts,
        event_type: "GO".into(),
        event_payload: Map::new(),
        from_states: strings(&["m.a"]),
        to_states: strings(&["m.b"]),
        actions: strings(&["act"]),
        disposition: Disposition::Transition,
        actor: Some("alice".into()),
        reason: None,
        correlation_id: None,
        machine_version: "1.0".into(),
        engine: false,
        error: None,
        origin: Origin::External,
    }
}

#[test]
fn log_append_read_and_next_seq() {
    let ns = ns!("log");
    let log = ns.log();
    assert_eq!(ok(log.next_seq("m")), 1);
    for seq in 1..=5 {
        ok(log.append(&record("m", seq, f64::from(u32::try_from(seq).unwrap_or(0)))));
    }
    assert_eq!(ok(log.next_seq("m")), 6);
    let all = ok(log.read("m", 0, 100));
    assert_eq!(all.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    assert_eq!(all[0], record("m", 1, 1.0));
    assert_eq!(ok(log.read("m", 2, 2)).iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4]);
    assert!(ok(log.read("m", 5, 10)).is_empty() && ok(log.read("m", 0, 0)).is_empty());
    assert_eq!(ok(log.read("other", 0, 10)), Vec::<TransitionRecord>::new());
    assert!(log.append(&record("m", 5, 1.0)).is_err(), "a seq at or below the top is refused");
    assert!(log.append(&record("m", 3, 1.0)).is_err());
    assert!(log.append(&record("m", 0, 1.0)).is_err());
    assert!(log.append(&record("m", 9, f64::NAN)).is_err());
    assert_eq!(ok(log.read("m", 0, 100)).len(), 5);
}

#[test]
fn log_layout_is_a_stream_with_the_seq_as_entry_id() {
    let ns = ns!("loglayout");
    let log = ns.log();
    ok(log.append(&record("m:1", 7, 12.5)));
    let rows: Vec<(String, HashMap<String, String>)> =
        ns.command(redis::cmd("XRANGE").arg(ns.key("log:m:1")).arg("-").arg("+"));
    assert_eq!(rows.len(), 1);
    let (id, fields) = &rows[0];
    assert_eq!(id, "7-0");
    assert_eq!((fields["seq"].as_str(), fields["ts"].as_str()), ("7", "12.5"));
    assert_eq!(fields["record"], record("m:1", 7, 12.5).to_json());
    assert!(fields["record"].contains(r#""seq": 7"#), "json.dumps spacing: {}", fields["record"]);
}

#[test]
fn log_append_next_never_mints_a_seq_twice_under_contention() {
    let ns = ns!("lograce");
    let log = Arc::new(ns.log());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let log = Arc::clone(&log);
            thread::spawn(move || {
                (0..10).map(|_| ok(log.append_next(record("m", 0, 1.0))).seq).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap_or_else(|_| panic!("worker panicked")))
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=80).collect::<Vec<u64>>());
    assert_eq!(ok(log.read("m", 0, 1000)).len(), 80);
}

#[test]
fn log_purge_forget_and_maxlen() {
    let ns = ns!("logretention");
    let log = ns.log();
    for seq in 1..=4 {
        ok(log.append(&record("a", seq, f64::from(u32::try_from(seq).unwrap_or(0)) * 10.0)));
    }
    ok(log.append(&record("b", 1, 5.0)));
    assert_eq!(ok(log.purge_older_than(25.0)), 3, "a:10, a:20 and b:5");
    assert_eq!(ok(log.read("a", 0, 10)).iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4]);
    assert_eq!(ok(log.read("b", 0, 10)), Vec::<TransitionRecord>::new());
    assert_eq!(ok(log.next_seq("b")), 1, "an emptied stream restarts at 1");
    assert!(log.purge_older_than(f64::NAN).is_err());
    assert_eq!(ok(log.forget("a")), 2);
    assert_eq!(ok(log.forget("a")), 0);

    let capped = ok(ok(RedisLog::open(&ns.url, &ns.prefix)).with_maxlen(5));
    for _ in 0..200 {
        ok(capped.append_next(record("capped", 0, 1.0)));
    }
    assert!(ok(capped.read("capped", 0, 1000)).len() < 200, "XADD MAXLEN ~ trimmed the head");
    assert_eq!(ok(capped.next_seq("capped")), 201);
    assert!(RedisLog::open(&ns.url, &ns.prefix).and_then(|l| l.with_maxlen(0)).is_err());
}

#[test]
fn log_corruption_is_reported() {
    let ns = ns!("logcorrupt");
    let log = ns.log();
    let add = |id: &str, record: &str| {
        ns.command::<String>(
            redis::cmd("XADD")
                .arg(ns.key("log:m"))
                .arg(id)
                .arg("record")
                .arg(record)
                .arg("ts")
                .arg("1"),
        );
    };
    add("1-0", &record("m", 1, 1.0).to_json());
    add("2-0", &record("m", 99, 1.0).to_json());
    assert!(
        matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(_))),
        "id disagrees with record"
    );
    ns.command::<i64>(redis::cmd("DEL").arg(ns.key("log:m")));
    add("1-0", "not json");
    assert!(matches!(log.read("m", 0, 10), Err(StoreError::Corrupt(_))));
}
