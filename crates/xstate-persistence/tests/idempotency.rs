//! Idempotency: fingerprint goldens, scope escaping, both inboxes, the guard and the ring.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use xstate_core::Event;
use xstate_persistence::StoreError;
#[cfg(feature = "sqlite")]
use xstate_persistence::idempotency::SqliteInbox;
use xstate_persistence::idempotency::{
    Admission, DEFAULT_TTL_S, IdempotencyGuard, InboxEntry, InboxErrorPolicy, InboxStore,
    MemoryInbox, PROCESSED_AT_KEY, PROCESSED_IDS_KEY, PROCESSED_RING_SIZE, RingView, default_key,
    fingerprint, processed_ids, record_processed, ring_contains, ring_evidence, scope,
    validate_idempotency_key, validate_principal,
};
use xstate_persistence::receipts::{ReceiptErrorKind, ReceiptJson, receipt_to_status};

const KEY_FIELDS: [&str; 2] = ["idempotency_key", "id"];

// ------------------------------------------------------------------------------- fingerprint
// Expected values computed with the Python library:
// `fingerprint(SimpleNamespace(type=..., payload=...))`.

#[test]
fn fingerprint_matches_python_goldens() {
    let payload = json!({"id": "evt_1", "amount": 10, "note": "h\u{e9}llo",
                         "nested": {"b": 1.5, "a": [1, 2]}});
    assert_eq!(
        fingerprint("PAY", &payload, &KEY_FIELDS),
        "3e6836cf1494b085042afb6748cbbe077166f0673b21a3de1e526750319ea7b8"
    );
    assert_eq!(
        fingerprint("PAY", &json!({"idempotency_key": "k", "id": "x", "amount": 10}), &KEY_FIELDS),
        "d12deb42f41a83276a83261a757b54c767b1d558fa39ce2490ed2fdebc809026"
    );
    assert_eq!(
        fingerprint("PING", &Value::Null, &KEY_FIELDS),
        "b01af89f573542746ebf70403abd6d7bcd97b3168617e4d54bf74ffb2eb09964",
        "a null payload counts as {{}}"
    );
}

#[test]
fn fingerprint_reproduces_python_float_and_string_canon() {
    let payload = json!({"a": 1e-7, "b": 1e16, "c": 100.0, "d": "\u{1F600}\n\"q\"\\",
                         "e": 0.1, "f": 123_456_789.125, "g": -0.0, "h": true, "i": null});
    assert_eq!(
        fingerprint("F", &payload, &KEY_FIELDS),
        "4a17cd29b14fa7034987ba542ce7c5decc7e39a75a17e06620cc1e76dbe407e3"
    );
}

#[test]
fn fingerprint_excludes_only_the_given_key_fields() {
    let payload = json!({"id": "evt_1", "amount": 10});
    assert_eq!(
        fingerprint("PAY", &payload, &["idempotency_key"]),
        "8aba5a859e2d9057788465615890a1635fce9725ab74d7712bbfb3df60b607e0"
    );
    assert_eq!(
        fingerprint("PAY", &payload, &KEY_FIELDS),
        fingerprint("PAY", &json!({"amount": 10, "id": "other"}), &KEY_FIELDS),
        "key fields and key order do not matter"
    );
    assert_ne!(
        fingerprint("PAY", &payload, &KEY_FIELDS),
        fingerprint("PAY", &json!({"amount": 11}), &KEY_FIELDS)
    );
    assert_ne!(
        fingerprint("PAY", &payload, &KEY_FIELDS),
        fingerprint("REFUND", &payload, &KEY_FIELDS)
    );
}

// ------------------------------------------------------------------------------ validation

#[test]
fn scope_escapes_each_part_so_the_join_is_injective() {
    assert_eq!(scope("alice", "order", "o-1"), "alice/order/o-1");
    assert_eq!(scope("a/m", "x%y", "k"), "a%2Fm/x%25y/k");
    assert_ne!(scope("a/m", "k", "z"), scope("a", "m/k", "z"));
    assert_ne!(scope("a%2F", "m", "k"), scope("a/", "m", "k"));
}

#[test]
fn idempotency_keys_are_bounded_printable_ascii() {
    for ok in ["evt_123", "a b", "~", &"x".repeat(255)] {
        validate_idempotency_key(ok).unwrap();
    }
    for bad in ["", "caf\u{e9}", "tab\there", "nl\n", "del\u{7f}", &"x".repeat(256)] {
        assert!(matches!(validate_idempotency_key(bad), Err(StoreError::InvalidKey(_))), "{bad:?}");
    }
}

#[test]
fn placeholder_principals_are_refused() {
    validate_principal("alice").unwrap();
    validate_principal("none").unwrap();
    for bad in ["", "None", "null", "anonymous"] {
        assert!(matches!(validate_principal(bad), Err(StoreError::InvalidKey(_))), "{bad:?}");
    }
}

#[test]
fn default_key_reads_idempotency_key_then_id() {
    let key = |p: Value| default_key(&Event::with_payload("E", p));
    assert_eq!(key(json!({"idempotency_key": "a", "id": "b"})).as_deref(), Some("a"));
    assert_eq!(key(json!({"id": "b"})).as_deref(), Some("b"));
    assert_eq!(key(json!({"id": 7})).as_deref(), Some("7"));
    assert_eq!(key(json!({"id": null})), None);
    assert_eq!(key(json!({"amount": 1})), None);
    assert_eq!(default_key(&Event::new("E")), None);
}

// ----------------------------------------------------------------------------------- inboxes

fn inboxes() -> Vec<(&'static str, Arc<dyn InboxStore>)> {
    #[cfg_attr(not(feature = "sqlite"), allow(unused_mut))]
    let mut all: Vec<(&'static str, Arc<dyn InboxStore>)> =
        vec![("memory", Arc::new(MemoryInbox::new()))];
    #[cfg(feature = "sqlite")]
    all.push(("sqlite", Arc::new(SqliteInbox::open_in_memory().unwrap())));
    all
}

#[test]
fn claim_mark_get_release_flow() {
    for (name, inbox) in inboxes() {
        assert_eq!(inbox.get("s", "k").unwrap(), None, "{name}");
        assert!(inbox.claim("s", "k", "fp1", Some(60.0)).unwrap(), "{name}");
        assert!(!inbox.claim("s", "k", "fp2", Some(60.0)).unwrap(), "{name}: first claim wins");

        let entry = inbox.get("s", "k").unwrap().unwrap();
        assert_eq!(entry.fingerprint, "fp1", "{name}");
        assert_eq!(entry.receipt_json, None, "{name}: in flight");
        assert!(entry.expires_at.is_some());

        inbox.mark("s", "k", r#"{"state_ids": []}"#, Some(60.0)).unwrap();
        let entry = inbox.get("s", "k").unwrap().unwrap();
        assert_eq!(entry.receipt_json.as_deref(), Some(r#"{"state_ids": []}"#), "{name}");
        assert_eq!(entry.fingerprint, "fp1", "{name}: mark keeps the fingerprint");

        inbox.release("s", "k").unwrap();
        assert!(inbox.get("s", "k").unwrap().is_some(), "{name}: release spares a marked entry");
    }
}

#[test]
fn release_drops_an_in_flight_claim_so_a_retry_can_claim() {
    for (name, inbox) in inboxes() {
        assert!(inbox.claim("s", "k", "fp", None).unwrap());
        inbox.release("s", "k").unwrap();
        assert_eq!(inbox.get("s", "k").unwrap(), None, "{name}");
        assert!(inbox.claim("s", "k", "fp", None).unwrap(), "{name}");
        assert_eq!(inbox.get("s", "k").unwrap().unwrap().expires_at, None, "{name}: ttl None");
    }
}

#[test]
fn scopes_and_keys_do_not_collide() {
    for (name, inbox) in inboxes() {
        assert!(inbox.claim("tenant-a", "evt", "fp", None).unwrap());
        assert!(inbox.claim("tenant-b", "evt", "fp", None).unwrap(), "{name}");
        assert!(inbox.claim("tenant-a", "evt2", "fp", None).unwrap(), "{name}");
    }
}

#[test]
fn an_expired_entry_is_invisible_and_claimable_again() {
    for (name, inbox) in inboxes() {
        assert!(inbox.claim("s", "k", "old", Some(0.0)).unwrap());
        assert_eq!(inbox.get("s", "k").unwrap(), None, "{name}: ttl 0 expires at once");
        assert!(inbox.claim("s", "k", "new", Some(60.0)).unwrap(), "{name}: expired is claimable");
        assert_eq!(inbox.get("s", "k").unwrap().unwrap().fingerprint, "new", "{name}");
    }
}

#[test]
fn purge_expired_removes_only_entries_expired_at_now() {
    for (name, inbox) in inboxes() {
        inbox.claim("s", "short", "fp", Some(10.0)).unwrap();
        inbox.claim("s", "long", "fp", Some(1000.0)).unwrap();
        inbox.claim("s", "forever", "fp", None).unwrap();
        assert_eq!(inbox.purge_expired(None).unwrap(), 0, "{name}: nothing is due yet");
        let later = xstate_persistence::store::wall_now() + 100.0;
        assert_eq!(inbox.purge_expired(Some(later)).unwrap(), 1, "{name}");
        assert_eq!(inbox.get("s", "short").unwrap(), None, "{name}");
        assert!(inbox.get("s", "long").unwrap().is_some(), "{name}");
        let far = later + 10_000.0;
        assert_eq!(inbox.purge_expired(Some(far)).unwrap(), 1, "{name}");
        assert!(inbox.get("s", "forever").unwrap().is_some(), "{name}: ttl None never expires");
    }
}

#[test]
fn forget_deletes_every_entry_of_one_scope() {
    for (name, inbox) in inboxes() {
        inbox.claim("a", "1", "fp", None).unwrap();
        inbox.claim("a", "2", "fp", None).unwrap();
        inbox.claim("b", "1", "fp", None).unwrap();
        assert_eq!(inbox.forget("a").unwrap(), 2, "{name}");
        assert_eq!(inbox.forget("a").unwrap(), 0, "{name}");
        assert!(inbox.get("b", "1").unwrap().is_some(), "{name}");
    }
}

#[test]
fn memory_inbox_mark_without_claim_stores_an_empty_fingerprint() {
    let inbox = MemoryInbox::new();
    assert_eq!(inbox.len(), 0);
    inbox.mark("s", "k", "{}", None).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        inbox.get("s", "k").unwrap(),
        Some(InboxEntry {
            fingerprint: String::new(),
            receipt_json: Some("{}".into()),
            expires_at: None
        })
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_inbox_persists_in_the_python_table_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("inbox.db");
    {
        let inbox = SqliteInbox::open(&path).unwrap();
        assert!(inbox.claim("s", "k", "fp", None).unwrap());
        inbox.mark("s", "k", "{}", None).unwrap();
    }
    let second = SqliteInbox::open(&path).unwrap();
    assert_eq!(second.get("s", "k").unwrap().unwrap().receipt_json.as_deref(), Some("{}"));
    assert!(!second.claim("s", "k", "fp", None).unwrap());

    let conn = rusqlite::Connection::open(&path).unwrap();
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(inbox)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(columns, ["scope", "key", "fingerprint", "receipt", "expires_at"]);
    let index: String = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='inbox_exp'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, "inbox_exp");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_claims_are_atomic_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = Arc::new(SqliteInbox::open(dir.path().join("race.db")).unwrap());
    let wins: usize = (0..8)
        .map(|_| {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || inbox.claim("s", "k", "fp", None).unwrap())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| usize::from(h.join().unwrap()))
        .sum();
    assert_eq!(wins, 1);
}

// ----------------------------------------------------------------------------------- guard

fn guard(inbox: &Arc<dyn InboxStore>) -> IdempotencyGuard {
    IdempotencyGuard::new(Arc::clone(inbox), "alice")
}

fn pay(id: &str, amount: u32) -> Event {
    Event::with_payload("PAY", json!({"id": id, "amount": amount}))
}

fn done_receipt() -> ReceiptJson {
    ReceiptJson { state_ids: vec!["m.b".into()], changed: true, ..ReceiptJson::default() }
}

#[test]
fn first_delivery_admits_then_in_flight_then_duplicate() {
    for (name, inbox) in inboxes() {
        let g = guard(&inbox);
        let scope = g.scope_for("order", "o-1").unwrap();
        assert_eq!(scope, "alice/order/o-1");

        let Admission::Admit(claim) = g.check(&scope, &pay("evt_1", 10)).unwrap() else {
            panic!("{name}: first delivery must be admitted");
        };
        assert!(claim.is_tracked());
        assert_eq!(claim.key(), Some("evt_1"));
        assert_eq!(claim.scope(), Some(scope.as_str()));

        assert!(
            matches!(g.check(&scope, &pay("evt_1", 10)).unwrap(), Admission::InFlight),
            "{name}: second delivery while the first runs"
        );
        claim.mark(&done_receipt()).unwrap();

        let Admission::Duplicate(cached) = g.check(&scope, &pay("evt_1", 10)).unwrap() else {
            panic!("{name}: a repeat after mark is a duplicate");
        };
        assert!(cached.duplicate && cached.changed, "{name}");
        assert_eq!(cached.state_ids, ["m.b"]);
        assert_eq!(receipt_to_status(&cached), 200, "duplicate keeps the original status");
    }
}

#[test]
fn same_key_with_a_different_payload_is_a_mismatch_even_in_flight() {
    for (name, inbox) in inboxes() {
        let g = guard(&inbox);
        let scope = g.scope_for("order", "o-1").unwrap();
        let Admission::Admit(claim) = g.check(&scope, &pay("evt_1", 10)).unwrap() else {
            panic!("{name}");
        };
        let different = g.check(&scope, &pay("evt_1", 99)).unwrap();
        assert!(matches!(different, Admission::Mismatch), "{name}: in flight");
        let refusal = different.refusal_receipt("evt_1", &["m.a".into()]).unwrap();
        assert_eq!(receipt_to_status(&refusal), 422);
        assert!(refusal.duplicate && !refusal.changed);
        assert_eq!(
            refusal.error.unwrap().message,
            "Idempotency key 'evt_1' was already used for a different event; keys must not be \
             reused with a different payload."
        );
        claim.mark(&done_receipt()).unwrap();
        assert!(
            matches!(g.check(&scope, &pay("evt_1", 99)).unwrap(), Admission::Mismatch),
            "{name}"
        );
        assert!(matches!(
            g.check(&scope, &Event::with_payload("REFUND", json!({"id": "evt_1", "amount": 10})))
                .unwrap(),
            Admission::Mismatch
        ));
    }
}

#[test]
fn in_flight_refusal_is_a_409_with_the_python_message() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    let scope = g.scope_for("m", "i").unwrap();
    let _held = g.check(&scope, &pay("e'1", 1)).unwrap();
    let in_flight = g.check(&scope, &pay("e'1", 1)).unwrap();
    let refusal = in_flight.refusal_receipt("e'1", &[]).unwrap();
    assert_eq!(receipt_to_status(&refusal), 409);
    assert_eq!(
        refusal.error.unwrap().message,
        r#"Idempotency key "e'1" is being processed; retry shortly."#,
        "Python repr picks double quotes for a key containing a single quote"
    );
    assert!(Admission::Duplicate(ReceiptJson::default()).refusal_receipt("k", &[]).is_none());
}

#[test]
fn tenants_with_the_same_key_never_collide() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let alice = IdempotencyGuard::new(Arc::clone(&inbox), "alice");
    let bob = IdempotencyGuard::new(Arc::clone(&inbox), "bob");
    let a = alice.scope_for("m", "i").unwrap();
    let b = bob.scope_for("m", "i").unwrap();
    assert!(matches!(alice.check(&a, &pay("evt", 1)).unwrap(), Admission::Admit(_)));
    assert!(matches!(bob.check(&b, &pay("evt", 2)).unwrap(), Admission::Admit(_)));
}

#[test]
fn events_without_a_key_are_admitted_untracked() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    for _ in 0..2 {
        let Admission::Admit(claim) = g.check("s", &Event::new("PING")).unwrap() else {
            panic!("unkeyed events are always admitted");
        };
        assert!(!claim.is_tracked());
        claim.mark(&done_receipt()).unwrap();
    }
}

#[test]
fn bad_keys_principals_and_ttls_are_errors() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    assert!(matches!(g.check("s", &pay("caf\u{e9}", 1)), Err(StoreError::InvalidKey(_))));
    let anonymous = IdempotencyGuard::new(Arc::clone(&inbox), "anonymous");
    assert!(matches!(anonymous.scope_for("m", "i"), Err(StoreError::InvalidKey(_))));
    for ttl in [Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
        let bad = IdempotencyGuard { ttl_s: ttl, ..guard(&inbox) };
        assert!(matches!(bad.check("s", &pay("k", 1)), Err(StoreError::InvalidKey(_))), "{ttl:?}");
    }
    assert_eq!(g.ttl_s, Some(DEFAULT_TTL_S));
}

#[test]
fn settle_releases_an_errored_unchanged_delivery_and_marks_everything_else() {
    for (name, inbox) in inboxes() {
        let g = guard(&inbox);
        let scope = g.scope_for("m", "i").unwrap();

        let Admission::Admit(claim) = g.check(&scope, &pay("e1", 1)).unwrap() else { panic!() };
        let failed = ReceiptJson {
            error: Some(xstate_persistence::receipts::ReceiptError::new(
                ReceiptErrorKind::Other("ActionError".into()),
                "boom",
            )),
            ..ReceiptJson::default()
        };
        claim.settle(&failed).unwrap();
        assert!(
            matches!(g.check(&scope, &pay("e1", 1)).unwrap(), Admission::Admit(_)),
            "{name}: the failed delivery did not take effect, so a retry is a first delivery"
        );

        let Admission::Admit(claim) = g.check(&scope, &pay("e2", 1)).unwrap() else { panic!() };
        claim.settle(&ReceiptJson { denied: true, ..ReceiptJson::default() }).unwrap();
        let Admission::Duplicate(cached) = g.check(&scope, &pay("e2", 1)).unwrap() else {
            panic!("{name}: a guard denial is that payload's answer");
        };
        assert!(cached.denied && cached.duplicate);
        assert_eq!(receipt_to_status(&cached), 409);
    }
}

#[test]
fn explicit_release_lets_a_retry_claim() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    let Admission::Admit(claim) = g.check("s", &pay("e", 1)).unwrap() else { panic!() };
    claim.release().unwrap();
    assert!(matches!(g.check("s", &pay("e", 1)).unwrap(), Admission::Admit(_)));
}

#[test]
fn marking_re_claims_when_the_row_vanished_while_the_event_ran() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    let Admission::Admit(claim) = g.check("s", &pay("e", 1)).unwrap() else { panic!() };
    inbox.forget("s").unwrap();
    claim.mark(&done_receipt()).unwrap();
    assert!(matches!(g.check("s", &pay("e", 1)).unwrap(), Admission::Duplicate(_)));
}

#[test]
fn an_expired_key_is_admitted_again() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = IdempotencyGuard { ttl_s: Some(0.0), ..guard(&inbox) };
    let Admission::Admit(claim) = g.check("s", &pay("e", 1)).unwrap() else { panic!() };
    claim.mark(&done_receipt()).unwrap();
    assert!(matches!(g.check("s", &pay("e", 1)).unwrap(), Admission::Admit(_)));
}

#[test]
fn a_malformed_cached_receipt_is_a_conservative_duplicate() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    let fp = fingerprint("PAY", &json!({"id": "e", "amount": 1}), &KEY_FIELDS);
    inbox.claim("s", "e", &fp, None).unwrap();
    inbox.mark("s", "e", "{truncated", None).unwrap();
    let Admission::Duplicate(receipt) = g.check("s", &pay("e", 1)).unwrap() else {
        panic!("the key WAS processed; never re-admit");
    };
    assert!(receipt.duplicate && receipt.error.is_none() && !receipt.changed);
}

#[derive(Debug)]
struct DeadInbox;

impl InboxStore for DeadInbox {
    fn get(&self, _: &str, _: &str) -> xstate_persistence::Result<Option<InboxEntry>> {
        Err(StoreError::Unavailable("down".into()))
    }
    fn claim(&self, _: &str, _: &str, _: &str, _: Option<f64>) -> xstate_persistence::Result<bool> {
        Err(StoreError::Unavailable("down".into()))
    }
    fn mark(&self, _: &str, _: &str, _: &str, _: Option<f64>) -> xstate_persistence::Result<()> {
        Err(StoreError::Unavailable("down".into()))
    }
    fn release(&self, _: &str, _: &str) -> xstate_persistence::Result<()> {
        Err(StoreError::Unavailable("down".into()))
    }
    fn purge_expired(&self, _: Option<f64>) -> xstate_persistence::Result<u64> {
        Err(StoreError::Unavailable("down".into()))
    }
    fn forget(&self, _: &str) -> xstate_persistence::Result<u64> {
        Err(StoreError::Unavailable("down".into()))
    }
}

#[test]
fn a_dead_inbox_refuses_by_default_and_admits_on_request() {
    let dead: Arc<dyn InboxStore> = Arc::new(DeadInbox);
    let refuse = guard(&dead);
    assert_eq!(refuse.on_inbox_error, InboxErrorPolicy::Refuse);
    let Err(StoreError::Unavailable(message)) = refuse.check("s", &pay("e", 1)) else {
        panic!("refuse mode answers 503");
    };
    assert_eq!(
        message,
        "Idempotency inbox unavailable while checking key 'e': StoreUnavailableError; retry later."
    );
    // The caller turns it into the receipt Python builds.
    let receipt = ReceiptJson {
        error: Some(xstate_persistence::receipts::ReceiptError::new(
            ReceiptErrorKind::InboxUnavailable,
            message,
        )),
        duplicate: true,
        ..ReceiptJson::default()
    };
    assert_eq!(receipt_to_status(&receipt), 503);

    let admit = IdempotencyGuard { on_inbox_error: InboxErrorPolicy::Admit, ..guard(&dead) };
    let Admission::Admit(claim) = admit.check("s", &pay("e", 1)).unwrap() else { panic!() };
    assert!(!claim.is_tracked(), "admitted without dedup");
    claim.mark(&done_receipt()).unwrap();
    // Unkeyed events never touch the inbox, so they are fine even when it is down.
    assert!(refuse.check("s", &Event::new("PING")).is_ok());
}

// -------------------------------------------------------------------------------------- ring

#[test]
fn the_ring_records_ids_and_evidence_and_is_bounded() {
    let mut ctx = json!({"count": 1});
    assert_eq!(processed_ids(&ctx).len(), 0);
    record_processed(&mut ctx, "s", "k1", "fp1", 1000.0);
    assert_eq!(ctx["count"], 1, "other context keys are untouched");
    assert_eq!(ctx[PROCESSED_IDS_KEY], json!(["s|k1"]));
    assert_eq!(ctx[PROCESSED_AT_KEY], json!({"s|k1": [1000.0, "fp1"]}));
    assert!(ring_contains(&ctx, "s|k1"));
    assert!(!ring_contains(&ctx, "s|k2"));

    for i in 0..PROCESSED_RING_SIZE + 6 {
        record_processed(&mut ctx, "s", &format!("n{i}"), "fp", 2000.0);
    }
    let ids = processed_ids(&ctx);
    assert_eq!(ids.len(), PROCESSED_RING_SIZE);
    assert_eq!(ids.last().map(String::as_str), Some("s|n69"));
    assert!(!ring_contains(&ctx, "s|k1"), "the oldest ids fall off the ring");
    assert_eq!(
        ctx[PROCESSED_AT_KEY].as_object().unwrap().len(),
        PROCESSED_RING_SIZE,
        "evidence is bounded like the ring"
    );

    let mut not_an_object = json!([1, 2]);
    record_processed(&mut not_an_object, "s", "k", "fp", 1.0);
    assert_eq!(not_an_object, json!([1, 2]), "a non-object context is left alone");
}

#[test]
fn ring_evidence_honours_the_ttl() {
    let mut ctx = json!({});
    record_processed(&mut ctx, "s", "k", "fp", 1000.0);
    assert_eq!(ring_evidence(&ctx, "s|k", Some(100.0), 1050.0).as_deref(), Some("fp"));
    assert_eq!(ring_evidence(&ctx, "s|k", Some(100.0), 1100.0), None, "a genuine expiry");
    assert_eq!(ring_evidence(&ctx, "s|k", None, 1e12).as_deref(), Some("fp"), "ttl None: forever");
    assert_eq!(ring_evidence(&ctx, "s|other", None, 1.0), None);
    let legacy = json!({PROCESSED_IDS_KEY: ["s|k"]});
    assert_eq!(ring_evidence(&legacy, "s|k", None, 1.0), None, "a pre-evidence snapshot");
}

fn ring_view<'a>(ctx: &'a Value, states: &'a [String], now: f64) -> RingView<'a> {
    RingView { context: ctx, state_ids: states, now_wall: now }
}

#[test]
fn a_crash_between_save_and_mark_is_repaired_from_the_snapshot() {
    for (name, inbox) in inboxes() {
        let g = guard(&inbox);
        let scope = g.scope_for("m", "i").unwrap();
        let event = pay("e", 1);
        let fp = fingerprint(&event.ty, &event.payload, &KEY_FIELDS);
        // First worker: claimed, the snapshot (with the ring) was saved, the mark never happened.
        let Admission::Admit(claim) = g.check(&scope, &event).unwrap() else { panic!() };
        let mut ctx = json!({});
        record_processed(&mut ctx, &scope, "e", &fp, 1000.0);
        drop(claim);

        let states = vec!["m.b".to_owned()];
        assert!(matches!(g.check(&scope, &event).unwrap(), Admission::InFlight), "{name}: blind");
        let Admission::Duplicate(receipt) =
            g.check_with_ring(&scope, &event, &ring_view(&ctx, &states, 1001.0)).unwrap()
        else {
            panic!("{name}: the snapshot says processed");
        };
        assert!(receipt.duplicate && !receipt.changed && receipt.error.is_none());
        assert_eq!(receipt.state_ids, ["m.b"]);
        let repaired = inbox.get(&scope, "e").unwrap().unwrap();
        assert!(repaired.receipt_json.is_some(), "{name}: the inbox was repaired");
        assert!(matches!(g.check(&scope, &event).unwrap(), Admission::Duplicate(_)), "{name}");
    }
}

#[test]
fn a_lost_inbox_entry_is_recovered_from_ring_evidence() {
    let inbox: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g = guard(&inbox);
    let scope = g.scope_for("m", "i").unwrap();
    let event = pay("e", 1);
    let fp = fingerprint(&event.ty, &event.payload, &KEY_FIELDS);
    let mut ctx = json!({});
    record_processed(&mut ctx, &scope, "e", &fp, 1000.0);
    let states = vec!["m.b".to_owned()];

    // Same payload: duplicate, and the fresh inbox is repaired.
    let view = ring_view(&ctx, &states, 1010.0);
    assert!(matches!(g.check_with_ring(&scope, &event, &view).unwrap(), Admission::Duplicate(_)));
    assert!(inbox.get(&scope, "e").unwrap().unwrap().receipt_json.is_some());

    // A different payload under the same key is a mismatch.
    let fresh: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g2 = guard(&fresh);
    let other = pay("e", 2);
    assert!(matches!(g2.check_with_ring(&scope, &other, &view).unwrap(), Admission::Mismatch));

    // Evidence older than the ttl is a genuine expiry: re-admitted.
    let third: Arc<dyn InboxStore> = Arc::new(MemoryInbox::new());
    let g3 = IdempotencyGuard { ttl_s: Some(5.0), ..guard(&third) };
    let late = ring_view(&ctx, &states, 1010.0);
    assert!(matches!(g3.check_with_ring(&scope, &event, &late).unwrap(), Admission::Admit(_)));
}

#[test]
fn the_ring_travels_with_the_context_json() {
    let mut ctx = json!({"orders": 3});
    record_processed(&mut ctx, "alice/m/i", "evt", "abc", 5.5);
    let text = serde_json::to_string(&ctx).unwrap();
    let back: Value = serde_json::from_str(&text).unwrap();
    assert!(ring_contains(&back, "alice/m/i|evt"));
    assert_eq!(ring_evidence(&back, "alice/m/i|evt", Some(10.0), 6.0).as_deref(), Some("abc"));
}
