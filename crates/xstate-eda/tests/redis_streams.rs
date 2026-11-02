//! Integration tests of the Redis Streams broker.
//!
//! They talk to the Redis at `XSM_REDIS_URL` (default `redis://127.0.0.1:6379/`), use a unique
//! key prefix per test, remove only their own keys, and skip with a printed note when Redis is not
//! reachable. The cross-language tests also need the Python virtualenv with the `redis` client.
#![cfg(feature = "redis-streams")]
// The skip notes are printed on purpose: a CI without Redis must say why nothing ran; `expect`
// states a precondition in the helpers of this integration-test crate (clippy.toml only exempts
// `#[test]` functions).
#![allow(clippy::print_stderr, clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};
use std::fmt::Display;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use xstate_eda::{
    BrokerError, Delivery, EdaError, Envelope, EnvelopeError, RedisStreamsBroker,
    RedisStreamsOptions,
};

const DEFAULT_URL: &str = "redis://127.0.0.1:6379/";
const TOPIC: &str = "orders";

fn ok<T, E: Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn redis_url() -> String {
    std::env::var("XSM_REDIS_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

fn raw_connection(url: &str) -> Option<redis::Connection> {
    redis::Client::open(url).ok()?.get_connection_with_timeout(Duration::from_secs(2)).ok()
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
        Some(Self { url, prefix: format!("xsmtest-{test}-{}-{nanos}", std::process::id()) })
    }

    fn options(&self) -> RedisStreamsOptions {
        RedisStreamsOptions::new(self.prefix.clone())
    }

    fn broker(&self, options: &RedisStreamsOptions) -> RedisStreamsBroker {
        ok(RedisStreamsBroker::open(&self.url, options))
    }

    fn stream(&self, topic: &str) -> String {
        format!("{}:stream:{topic}", self.prefix)
    }

    fn query<T: redis::FromRedisValue>(&self, cmd: &mut redis::Cmd) -> T {
        let mut conn = raw_connection(&self.url).unwrap_or_else(|| panic!("Redis went away"));
        ok(cmd.query(&mut conn))
    }

    fn xlen(&self, stream: &str) -> u64 {
        self.query(redis::cmd("XLEN").arg(stream))
    }

    fn pending(&self, stream: &str, group: &str) -> usize {
        let rows: Vec<(String, String, u64, u64)> =
            self.query(redis::cmd("XPENDING").arg(stream).arg(group).arg("-").arg("+").arg(1000));
        rows.len()
    }

    /// The entries of `stream` as `(id, fields)`.
    fn entries(&self, stream: &str) -> Vec<(String, HashMap<String, String>)> {
        self.query(redis::cmd("XRANGE").arg(stream).arg("-").arg("+"))
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

fn envelope(subject: &str, n: i64) -> Envelope {
    ok(Envelope::new("xsm.order.GO").subject(subject).data(json!({ "n": n })).build())
}

#[test]
fn options_and_urls_are_validated() {
    let open = |o: &RedisStreamsOptions| RedisStreamsBroker::open(&redis_url(), o);
    let bad = |o: RedisStreamsOptions| matches!(open(&o), Err(EdaError::InvalidArgument(_)));
    assert!(bad(RedisStreamsOptions::new("")));
    assert!(bad(RedisStreamsOptions::new("a b")));
    assert!(bad(RedisStreamsOptions::new("p").shards(0)));
    assert!(bad(RedisStreamsOptions::new("p").batch(0)));
    assert!(bad(RedisStreamsOptions::new("p").group("")));
    assert!(bad(RedisStreamsOptions::new("p").maxlen(0)));
    assert!(matches!(
        RedisStreamsBroker::open("not a url", &RedisStreamsOptions::new("p")),
        Err(EdaError::InvalidArgument(_))
    ));
}

mod sync_api {
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    /// `n` deliveries of `topic`, each polled within two seconds.
    fn take(broker: &RedisStreamsBroker, topic: &str, n: usize) -> Vec<Delivery> {
        (0..n)
            .map(|_| {
                ok(broker.poll(topic, Some(Duration::from_secs(2)))).expect("a delivery in time")
            })
            .collect()
    }

    fn idle(broker: &RedisStreamsBroker, topic: &str) -> bool {
        ok(broker.poll(topic, Some(Duration::ZERO))).is_none()
    }

    #[test]
    fn an_unreachable_redis_fails_publish_and_marks_the_broker_unhealthy() {
        let broker =
            ok(RedisStreamsBroker::open("redis://127.0.0.1:1/", &RedisStreamsOptions::new("p")));
        assert!(broker.healthy());
        assert!(matches!(broker.publish(TOPIC, &envelope("s", 1)), Err(BrokerError::Publish(_))));
        assert!(!broker.healthy());
        assert!(matches!(broker.poll(TOPIC, Some(Duration::ZERO)), Err(BrokerError::Other(_))));
    }

    #[test]
    fn publish_writes_the_python_entry_layout() {
        let ns = ns!("layout");
        let broker = ns.broker(&ns.options());
        let (a, b) = (envelope("order:1", 1), ok(Envelope::new("xsm.order.GO").build()));
        ok(broker.publish(TOPIC, &a));
        ok(broker.publish(TOPIC, &b));
        let rows = ns.entries(&ns.stream(TOPIC));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1["ce"], ok(a.to_json(1 << 20)));
        assert_eq!(rows[0].1["subject"], "order:1");
        assert_eq!(rows[1].1["subject"], "", "no subject is the empty string");
        assert_eq!(rows[0].1.len(), 2, "exactly the fields ce and subject");
    }

    #[test]
    fn poll_ack_settles_the_entry_and_a_second_ack_is_a_no_op() {
        let ns = ns!("ack");
        let broker = ns.broker(&ns.options());
        let sent = envelope("s", 1);
        assert!(idle(&broker, TOPIC));
        ok(broker.publish(TOPIC, &sent));
        let delivery = take(&broker, TOPIC, 1).remove(0);
        assert_eq!(delivery.envelope, sent);
        assert_eq!((delivery.envelope.attempt(), delivery.topic.as_str()), (0, TOPIC));
        assert_eq!((ns.pending(&ns.stream(TOPIC), "xsm"), broker.in_flight()), (1, 1));
        ok(broker.ack(&delivery));
        ok(broker.ack(&delivery));
        assert_eq!((ns.pending(&ns.stream(TOPIC), "xsm"), broker.in_flight()), (0, 0));
        assert!(idle(&broker, TOPIC));
    }

    #[test]
    fn an_idle_poll_waits_for_its_timeout_and_a_blocked_poll_sees_new_traffic() {
        let ns = ns!("idle");
        let broker = ns.broker(&ns.options());
        let started = Instant::now();
        assert!(ok(broker.poll(TOPIC, Some(Duration::from_millis(300)))).is_none());
        assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());

        let publisher = ns.broker(&ns.options().consumer("publisher"));
        let sent = envelope("s", 7);
        let to_send = sent.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            ok(publisher.publish(TOPIC, &to_send));
        });
        let got = ok(broker.poll(TOPIC, Some(Duration::from_secs(3)))).expect("woken by traffic");
        assert_eq!(got.envelope, sent);
        handle.join().unwrap_or_else(|_| panic!("publisher panicked"));
    }

    #[test]
    fn nack_requeues_at_the_head_with_the_attempt_bumped_and_the_entry_stays_pending() {
        let ns = ns!("nack");
        let broker = ns.broker(&ns.options());
        let (a, b) = (envelope("s", 1), envelope("s", 2));
        ok(broker.publish(TOPIC, &a));
        ok(broker.publish(TOPIC, &b));
        let mut first = take(&broker, TOPIC, 1).remove(0);
        first.envelope.data = json!({"n": 99}); // a consumer mutating its copy changes nothing
        ok(broker.nack(&first, true));
        ok(broker.nack(&first, true)); // settling twice is a no-op
        assert_eq!(ns.pending(&ns.stream(TOPIC), "xsm"), 2, "nothing was acked");

        let again = take(&broker, TOPIC, 1).remove(0);
        assert_eq!((again.envelope.id.as_str(), again.envelope.attempt()), (a.id.as_str(), 1));
        assert_eq!(again.envelope.data, json!({"n": 1}));
        ok(broker.nack(&again, true));
        let third = take(&broker, TOPIC, 1).remove(0);
        assert_eq!((third.envelope.id.as_str(), third.envelope.attempt()), (a.id.as_str(), 2));
        ok(broker.ack(&third));
        let second = take(&broker, TOPIC, 1).remove(0);
        assert_eq!((second.envelope.id.as_str(), second.envelope.attempt()), (b.id.as_str(), 0));

        // nack without requeue settles it: acked natively, never redelivered
        ok(broker.nack(&second, false));
        assert_eq!(ns.pending(&ns.stream(TOPIC), "xsm"), 0);
        assert!(idle(&broker, TOPIC));
    }

    #[test]
    fn the_wire_attempt_is_never_trusted() {
        let ns = ns!("wireattempt");
        let broker = ns.broker(&ns.options());
        ok(broker.publish(TOPIC, &envelope("s", 1).with_attempt(10)));
        assert_eq!(take(&broker, TOPIC, 1)[0].envelope.attempt(), 0);
    }

    #[test]
    fn consumers_of_one_group_share_a_topic_and_other_groups_get_their_own_copy() {
        let ns = ns!("group");
        let opts = ns.options().batch(1);
        let (c1, c2) = (ns.broker(&opts.clone().consumer("c1")), ns.broker(&opts.consumer("c2")));
        let mut sent = BTreeSet::new();
        for n in 0..6 {
            let e = envelope("s", n);
            sent.insert(e.id.clone());
            ok(c1.publish(TOPIC, &e));
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            for broker in [&c1, &c2] {
                let d = take(broker, TOPIC, 1).remove(0);
                ok(broker.ack(&d));
                got.push(d.envelope.id);
            }
        }
        assert_eq!(got.len(), 6);
        assert_eq!(got.iter().cloned().collect::<BTreeSet<_>>(), sent, "no duplicates, none lost");
        assert!(idle(&c1, TOPIC) && idle(&c2, TOPIC));

        // a group created later still starts at id 0: nothing published earlier is lost
        let other = ns.broker(&ns.options().group("audit"));
        assert_eq!(take(&other, TOPIC, 6).len(), 6);
    }

    #[test]
    fn a_crashed_consumers_entries_are_reclaimed_with_the_attempt_counted() {
        let ns = ns!("reclaim");
        let opts = ns.options().min_idle_ms(300);
        let doomed = ns.broker(&opts.clone().consumer("doomed"));
        let rescuer = ns.broker(&opts.consumer("rescuer"));
        let sent = envelope("s", 1);
        ok(doomed.publish(TOPIC, &sent));
        let lost = take(&doomed, TOPIC, 1).remove(0);
        assert_eq!(lost.envelope.attempt(), 0);
        assert!(idle(&rescuer, TOPIC), "not idle long enough to claim");
        // the consumer that still holds it never reclaims it from itself
        std::thread::sleep(Duration::from_millis(400));
        assert!(idle(&doomed, TOPIC));

        let saved = take(&rescuer, TOPIC, 1).remove(0);
        assert_eq!((saved.envelope.id.as_str(), saved.envelope.attempt()), (sent.id.as_str(), 1));
        ok(rescuer.ack(&saved));
        assert_eq!(ns.pending(&ns.stream(TOPIC), "xsm"), 0);
        assert!(idle(&rescuer, TOPIC));
    }

    #[test]
    fn messages_held_locally_too_long_are_released_to_the_broker_clock() {
        let ns = ns!("release");
        let broker = ns.broker(&ns.options().min_idle_ms(400));
        for n in 0..3 {
            ok(broker.publish(TOPIC, &envelope("s", n)));
        }
        let first = take(&broker, TOPIC, 1).remove(0);
        assert_eq!(broker.held(TOPIC), 2, "the batch fetched three, one was delivered");
        std::thread::sleep(Duration::from_millis(250)); // past half the redelivery window
        assert!(idle(&broker, TOPIC), "stale local entries are released, not delivered late");
        assert_eq!(broker.held(TOPIC), 0);
        std::thread::sleep(Duration::from_millis(250));
        // now idle for the full window: the broker redelivers them, counting the first delivery
        let redelivered = take(&broker, TOPIC, 2);
        assert!(redelivered.iter().all(|d| d.envelope.attempt() == 1));
        ok(broker.ack(&first));
    }

    #[test]
    fn subscribe_ends_after_the_topic_stays_idle() {
        let ns = ns!("subscribe");
        let broker = ns.broker(&ns.options());
        for n in 0..5 {
            ok(broker.publish(TOPIC, &envelope("s", n)));
        }
        let mut seen = Vec::new();
        for delivery in broker.subscribe(TOPIC, Some(Duration::from_millis(300))) {
            let delivery = ok(delivery);
            seen.push(delivery.envelope.data["n"].as_i64().unwrap_or(-1));
            ok(broker.ack(&delivery));
        }
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_subject_always_hashes_to_one_shard() {
        let ns = ns!("shards");
        let broker = ns.broker(&ns.options().shards(3));
        for n in 0..4 {
            ok(broker.publish(TOPIC, &envelope("same-subject", n)));
        }
        let streams: Vec<String> = (0..3).map(|n| format!("{}:{n}", ns.stream(TOPIC))).collect();
        let lengths: Vec<u64> = streams.iter().map(|s| ns.xlen(s)).collect();
        assert_eq!(lengths.iter().sum::<u64>(), 4);
        assert_eq!(lengths.iter().filter(|n| **n == 4).count(), 1, "one shard: {lengths:?}");
        let got: Vec<i64> = take(&broker, TOPIC, 4)
            .iter()
            .map(|d| d.envelope.data["n"].as_i64().unwrap_or(-1))
            .collect();
        assert_eq!(got, [0, 1, 2, 3], "per-subject order");

        for n in 0..30 {
            ok(broker.publish("spread", &envelope(&format!("subject-{n}"), n)));
        }
        let used = (0..3).filter(|n| ns.xlen(&format!("{}:{n}", ns.stream("spread"))) > 0).count();
        assert!(used >= 2, "30 subjects spread over the shards");
    }

    #[test]
    fn the_size_cap_applies_to_publish_and_to_inbound_entries() {
        let ns = ns!("cap");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_seen = std::sync::Arc::clone(&seen);
        let options =
            ns.options().max_bytes(400).on_undecodable(move |topic, error: &EnvelopeError| {
                hook_seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((topic.to_owned(), error.type_name()));
            });
        let broker = ns.broker(&options);
        let big = ok(Envelope::new("xsm.o.GO").data(json!({"blob": "x".repeat(1000)})).build());
        assert!(matches!(
            broker.publish(TOPIC, &big),
            Err(BrokerError::Envelope(EnvelopeError::TooLarge(_)))
        ));
        assert_eq!(ns.xlen(&ns.stream(TOPIC)), 0, "nothing was published");

        // entries other writers planted: oversized, not JSON, no `ce` field
        let stream = ns.stream(TOPIC);
        let plant = |fields: &[(&str, String)]| {
            let mut cmd = redis::cmd("XADD");
            cmd.arg(&stream).arg("*");
            for (name, value) in fields {
                cmd.arg(*name).arg(value);
            }
            ns.query::<String>(&mut cmd);
        };
        plant(&[("ce", "x".repeat(1000))]);
        plant(&[("ce", "not json".to_owned())]);
        plant(&[("subject", "orphan".to_owned())]);
        let good = envelope("s", 1);
        ok(broker.publish(TOPIC, &good));
        let delivery = take(&broker, TOPIC, 1).remove(0);
        assert_eq!(delivery.envelope, good);
        assert!(idle(&broker, TOPIC));
        assert_eq!(
            ns.pending(&stream, "xsm"),
            1,
            "the bad entries were acked, only the good one pends"
        );
        let reports = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|(topic, _)| topic == TOPIC));
        assert_eq!(reports[0].1, "EnvelopeTooLargeError");
    }

    #[test]
    fn maxlen_trims_streams_on_publish() {
        let ns = ns!("maxlen");
        let broker = ns.broker(&ns.options().maxlen(5));
        for n in 0..400 {
            ok(broker.publish(TOPIC, &envelope("s", n)));
        }
        let len = ns.xlen(&ns.stream(TOPIC));
        assert!((5..400).contains(&len), "approximate trimming kept {len}");
    }
}

mod async_api {
    use xstate_eda::BrokerAdapter;

    use super::*;

    #[tokio::test]
    async fn publish_poll_nack_ack_work_on_the_blocking_pool() {
        let ns = ns!("async");
        let broker = ns.broker(&ns.options());
        let sent = envelope("order:1", 1);
        ok(BrokerAdapter::publish(&broker, TOPIC, &sent).await);
        let first = ok(BrokerAdapter::poll(&broker, TOPIC, Some(Duration::from_secs(2))).await)
            .expect("delivered");
        assert_eq!(first.envelope, sent);
        ok(BrokerAdapter::nack(&broker, &first, true).await);
        let again = ok(BrokerAdapter::poll(&broker, TOPIC, Some(Duration::from_secs(2))).await)
            .expect("redelivered");
        assert_eq!(again.envelope.attempt(), 1);
        ok(BrokerAdapter::ack(&broker, &again).await);
        assert_eq!(ns.pending(&ns.stream(TOPIC), "xsm"), 0);
        assert!(ok(BrokerAdapter::poll(&broker, TOPIC, Some(Duration::ZERO)).await).is_none());

        // settled through the other trait: one delivery handle works with both
        ok(BrokerAdapter::publish(&broker, TOPIC, &envelope("s", 2)).await);
        let third = ok(BrokerAdapter::poll(&broker, TOPIC, Some(Duration::from_secs(2))).await)
            .expect("delivered");
        ok(BrokerAdapter::nack(&broker, &third, false).await);
        assert_eq!(ns.pending(&ns.stream(TOPIC), "xsm"), 0);
    }

    #[tokio::test]
    async fn a_poll_dropped_mid_wait_loses_nothing() {
        let ns = ns!("cancel");
        let broker = ns.broker(&ns.options());
        // cancelled while blocked in Redis ...
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            BrokerAdapter::poll(&broker, TOPIC, Some(Duration::from_secs(30))),
        )
        .await;
        assert!(cancelled.is_err());
        // ... and the message published afterwards is still delivered
        let sent = envelope("s", 1);
        ok(BrokerAdapter::publish(&broker, TOPIC, &sent).await);
        let got = ok(BrokerAdapter::poll(&broker, TOPIC, Some(Duration::from_secs(3))).await)
            .expect("delivered");
        assert_eq!(got.envelope, sent);
    }
}

/// Redis shared with the Python library: each runtime publishes, the other consumes, with the
/// same consumer group and the same shard hashing. Skipped (with a note) when the Python
/// virtualenv or its `redis` client is missing; override the interpreter with `XSM_PYTHON`.
mod cross_language {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;
    use xstate_eda::SyncBrokerAdapter;

    use super::*;

    const SHARDS: usize = 4;
    const GROUP: &str = "xl";
    const PY_SUBJECTS: [&str; 4] = ["order:1", "order:2", "ключ-3", "日本/4"];
    const RUST_SUBJECTS: [&str; 4] = ["rust:a", "rust:b", "ключ-c", "😀-d"];

    const PY_IMPORTS: &str = "
import sys, json
from xstate_statemachine.contrib.brokers.redis_streams import SyncRedisStreamsBroker
from xstate_statemachine.eda.envelope import Envelope
url, prefix, shards, group = sys.argv[1:5]
broker = SyncRedisStreamsBroker(url=url, prefix=prefix, group=group, consumer='py', shards=int(shards))
streams = lambda subjects: {s: broker.transport.stream(sys.argv[5], broker.transport.shard_of(s)) for s in subjects}
";

    /// Python publishes one envelope per subject and prints which stream each subject used.
    const PY_PUBLISH: &str = "
subjects = json.loads(sys.argv[6])
for i, subject in enumerate(subjects):
    broker.publish(sys.argv[5], Envelope.new(type='xsm.order.GO', subject=subject,
                   data={'n': i, 't': '\\u00e9'}, correlationid='corr-' + str(i)))
print(json.dumps(streams(subjects)))
";

    /// Python consumes (and acks) everything on the topic, then prints it with the stream mapping.
    const PY_CONSUME: &str = "
subjects = json.loads(sys.argv[6])
got = []
for d in broker.subscribe(sys.argv[5], timeout=1.0):
    attempt = d.envelope.attempt
    got.append({'id': d.envelope.id, 'type': d.envelope.type, 'subject': d.envelope.subject,
                'data': d.envelope.data, 'attempt': attempt() if callable(attempt) else attempt})
    d.ack()
print(json.dumps({'got': got, 'streams': streams(subjects)}))
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
        fn command(&self, script: &str, topic: &str, subjects: &[&str]) -> Command {
            let mut cmd = Command::new(&self.exe);
            cmd.arg("-I")
                .arg("-c")
                .arg(format!("{PY_IMPORTS}{script}"))
                .args([&self.url, &self.prefix, &SHARDS.to_string(), GROUP, topic])
                .arg(json!(subjects).to_string());
            cmd
        }

        fn run(&self, script: &str, topic: &str, subjects: &[&str]) -> Value {
            let out = self
                .command(script, topic, subjects)
                .output()
                .unwrap_or_else(|e| panic!("python: {e}"));
            assert!(
                out.status.success(),
                "python failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            ok(serde_json::from_slice(&out.stdout))
        }
    }

    fn usable_python(ns: &Ns) -> Option<Py> {
        let Some(exe) = python() else {
            eprintln!(
                "skipping Redis Streams cross-language test: no Python virtualenv (set XSM_PYTHON)"
            );
            return None;
        };
        let py = Py { exe, url: ns.url.clone(), prefix: ns.prefix.clone() };
        let probe = py.command("", TOPIC, &[]).output().ok()?;
        if probe.status.success() {
            return Some(py);
        }
        eprintln!(
            "skipping Redis Streams cross-language test: {}",
            String::from_utf8_lossy(&probe.stderr).lines().last().unwrap_or("python import failed")
        );
        None
    }

    /// Where each subject actually landed: `subject -> stream key`, read off the shard streams.
    fn landed(ns: &Ns, topic: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for shard in 0..SHARDS {
            let stream = format!("{}:{shard}", ns.stream(topic));
            for (_, fields) in ns.entries(&stream) {
                out.insert(fields["subject"].clone(), stream.clone());
            }
        }
        out
    }

    #[test]
    fn python_publishes_rust_consumes_then_rust_publishes_python_consumes() {
        let ns = ns!("xlang");
        let Some(py) = usable_python(&ns) else { return };
        let broker = ns.broker(&ns.options().group(GROUP).consumer("rust").shards(SHARDS));

        // Python -> Rust: same shard for every subject, same envelope fields
        let py_streams = py.run(PY_PUBLISH, "py-events", &PY_SUBJECTS);
        let placed = landed(&ns, "py-events");
        for subject in PY_SUBJECTS {
            assert_eq!(py_streams[subject], placed[subject].as_str(), "{subject}");
        }
        let mut got = Vec::new();
        while let Some(d) = ok(broker.poll("py-events", Some(Duration::from_millis(500)))) {
            assert_eq!(d.envelope.attempt(), 0);
            ok(broker.ack(&d));
            got.push(d.envelope);
        }
        assert_eq!(got.len(), PY_SUBJECTS.len());
        for e in &got {
            let i = PY_SUBJECTS
                .iter()
                .position(|s| Some(*s) == e.subject.as_deref())
                .expect("known subject");
            assert_eq!(e.ty, "xsm.order.GO");
            assert_eq!(e.data, json!({"n": i, "t": "é"}));
            assert_eq!(e.correlationid.as_deref(), Some(format!("corr-{i}").as_str()));
        }
        for shard in 0..SHARDS {
            assert_eq!(ns.pending(&format!("{}:{shard}", ns.stream("py-events")), GROUP), 0);
        }

        // Rust -> Python
        let sent: Vec<Envelope> =
            RUST_SUBJECTS.iter().zip(0..).map(|(subject, n)| envelope(subject, n)).collect();
        for e in &sent {
            ok(broker.publish("rust-events", e));
        }
        let placed = landed(&ns, "rust-events");
        let seen = py.run(PY_CONSUME, "rust-events", &RUST_SUBJECTS);
        let consumed = seen["got"].as_array().expect("list");
        assert_eq!(consumed.len(), sent.len());
        for e in &sent {
            let row = consumed.iter().find(|r| r["id"] == e.id.as_str()).expect("consumed by id");
            assert_eq!(row["type"], e.ty.as_str());
            assert_eq!(row["subject"], e.subject.as_deref().unwrap_or_default());
            assert_eq!(row["data"], e.data);
            assert_eq!(row["attempt"], 0);
        }
        for subject in RUST_SUBJECTS {
            assert_eq!(seen["streams"][subject], placed[subject].as_str(), "{subject}");
        }
        for shard in 0..SHARDS {
            assert_eq!(ns.pending(&format!("{}:{shard}", ns.stream("rust-events")), GROUP), 0);
        }
    }
}
