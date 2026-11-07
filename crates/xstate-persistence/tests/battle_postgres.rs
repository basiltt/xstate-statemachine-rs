#![cfg(feature = "postgres")]
// The skip note is printed on purpose, mirroring `postgres.rs`.
#![allow(clippy::print_stderr, clippy::unwrap_used, clippy::expect_used)]
//! Adversarial PostgreSQL additions for the §6.2 matrix. `postgres.rs` covers the store, locks,
//! inbox and log; this file adds the planted-invalid-key `list_keys` limit edge (a regression the
//! SQLite store shares) and a ~50-writer concurrency check.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::runtime::{Builder, Runtime};
use xstate_persistence::{PostgresStore, PostgresStoreOptions, SaveOptions, StateStore};

const DEFAULT_URL: &str = "postgres://xsm:xsm@127.0.0.1:5432/xsm";

fn postgres_url() -> String {
    std::env::var("XSM_POSTGRES_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

fn ok<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

struct Raw {
    rt: Runtime,
    pool: PgPool,
}

impl Raw {
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

    fn exec(&self, sql: &str) {
        ok(self.rt.block_on(sqlx::query(AssertSqlSafe(sql.to_owned())).execute(&self.pool)));
    }
}

/// One throw-away schema, dropped at the end of the test.
struct Ns {
    url: String,
    schema: String,
    raw: Raw,
}

impl Ns {
    fn new() -> Option<Self> {
        let url = postgres_url();
        let Some(raw) = Raw::connect(&url) else {
            common::service_unavailable(
                "battle_postgres",
                &format!("no PostgreSQL reachable (XSM_POSTGRES_URL, default {DEFAULT_URL})"),
            );
            return None;
        };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let schema = format!("xsmbattle_{}_{nanos}", std::process::id());
        raw.exec(&format!("CREATE SCHEMA {schema}"));
        Some(Self { url, schema, raw })
    }

    fn store(&self) -> PostgresStore {
        ok(PostgresStore::connect(
            &self.url,
            &PostgresStoreOptions::default().schema(self.schema.clone()),
        ))
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema);
        let _ = self.raw.rt.block_on(sqlx::query(AssertSqlSafe(sql)).execute(&self.raw.pool));
    }
}

#[test]
fn list_keys_filters_invalid_keys_before_applying_the_limit() {
    let Some(ns) = Ns::new() else { return };
    let store = ns.store();
    ok(store.save("a", "{}", &SaveOptions::default()));
    ok(store.save("b", "{}", &SaveOptions::default()));
    // A foreign writer plants a key the store would refuse; '' sorts before everything.
    ns.raw
        .exec(&format!("INSERT INTO {}.xsm_snapshots VALUES ('', '{{}}', 1, '', 1.0)", ns.schema));
    assert_eq!(
        ok(store.list_keys("", 1)),
        ["a".to_owned()],
        "an invalid planted key must be filtered out before the LIMIT is applied"
    );
}

#[test]
fn fifty_optimistic_creators_have_exactly_one_winner() {
    let Some(ns) = Ns::new() else { return };
    let store = Arc::new(ns.store());
    let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let threads: Vec<_> = (0..50)
        .map(|i| {
            let (store, wins) = (Arc::clone(&store), Arc::clone(&wins));
            thread::spawn(move || {
                let options = SaveOptions { expected_version: Some(0), ..SaveOptions::default() };
                match store.save("race", &format!("w{i}"), &options) {
                    Ok(1) => {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(xstate_persistence::StoreError::Conflict { .. }) => {}
                    other => panic!("unexpected {other:?}"),
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap_or_else(|_| panic!("worker panicked"));
    }
    assert_eq!(wins.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(ok(store.load("race")).expect("saved").version, 1);
}
