//! Receipt JSON golden strings (produced by the Python library) and the status mapping.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use xstate_core::{Machine, MachineLogic, SyncInterpreter};
use xstate_persistence::StoreError;
use xstate_persistence::receipts::{
    ReceiptError, ReceiptErrorKind, ReceiptFlags, ReceiptJson, STATUS_ACCEPTED, STATUS_CONFLICT,
    STATUS_ERROR, STATUS_NO_OP, STATUS_OK, STATUS_UNAVAILABLE, STATUS_UNPROCESSABLE,
    receipt_to_status,
};

/// `json.dumps(receipt_to_json(Receipt(frozenset(["m.b", "m.a"]), True, None)))`.
const GOLDEN_PLAIN: &str = r#"{"state_ids": ["m.a", "m.b"], "changed": true, "error": null, "deferred": false, "denied": false, "duplicate": false}"#;
/// Python, with a non-ASCII id, an error whose message has quotes, `denied` and `duplicate`.
const GOLDEN_ERROR: &str = r#"{"state_ids": ["m.\u00e9"], "changed": false, "error": {"type": "IdempotencyMismatchError", "message": "bad \"key\""}, "deferred": false, "denied": true, "duplicate": true}"#;

fn receipt(error: Option<ReceiptErrorKind>) -> ReceiptJson {
    ReceiptJson {
        state_ids: vec!["m.a".into()],
        error: error.map(|kind| ReceiptError::new(kind, "x")),
        ..ReceiptJson::default()
    }
}

#[test]
fn to_json_matches_the_python_golden_strings() {
    let plain = ReceiptJson {
        state_ids: vec!["m.b".into(), "m.a".into()],
        changed: true,
        ..ReceiptJson::default()
    };
    assert_eq!(plain.to_json(), GOLDEN_PLAIN, "state_ids are sorted on output");

    let with_error = ReceiptJson {
        state_ids: vec!["m.\u{e9}".into()],
        error: Some(ReceiptError::new(ReceiptErrorKind::IdempotencyMismatch, "bad \"key\"")),
        denied: true,
        duplicate: true,
        ..ReceiptJson::default()
    };
    assert_eq!(with_error.to_json(), GOLDEN_ERROR);
}

#[test]
fn golden_strings_decode_and_round_trip() {
    let plain = ReceiptJson::from_json(GOLDEN_PLAIN).unwrap();
    assert_eq!(plain.state_ids, ["m.a", "m.b"]);
    assert!(plain.changed && plain.error.is_none());
    assert_eq!(plain.to_json(), GOLDEN_PLAIN);

    let with_error = ReceiptJson::from_json(GOLDEN_ERROR).unwrap();
    let error = with_error.error.as_ref().unwrap();
    assert_eq!(error.kind, ReceiptErrorKind::IdempotencyMismatch);
    assert_eq!(error.message, "bad \"key\"");
    assert_eq!(with_error.to_json(), GOLDEN_ERROR);
}

#[test]
fn unknown_error_types_survive_a_round_trip() {
    let text = r#"{"state_ids": [], "changed": false, "error": {"type": "TransitionFailedError", "message": "boom"}, "deferred": false, "denied": false, "duplicate": false}"#;
    let receipt = ReceiptJson::from_json(text).unwrap();
    assert_eq!(
        receipt.error.as_ref().unwrap().kind,
        ReceiptErrorKind::Other("TransitionFailedError".into())
    );
    assert_eq!(receipt.to_json(), text);
}

#[test]
fn missing_flags_default_to_false() {
    let receipt = ReceiptJson::from_json(r#"{"state_ids": ["m.a"]}"#).unwrap();
    assert_eq!(receipt, ReceiptJson { state_ids: vec!["m.a".into()], ..ReceiptJson::default() });
}

#[test]
fn malformed_records_are_corrupt_not_panics() {
    for bad in [
        "not json",
        "[]",
        r#"{"state_ids": "m.a"}"#,
        r#"{"state_ids": [1]}"#,
        r"{}",
        r#"{"state_ids": [], "error": "boom"}"#,
        r#"{"state_ids": [], "error": {"type": "X"}}"#,
        r#"{"state_ids": [], "error": {"type": 1, "message": "m"}}"#,
        r#"{"state_ids": [], "changed": "yes"}"#,
        r#"{"state_ids": [], "duplicate": 1}"#,
    ] {
        assert!(matches!(ReceiptJson::from_json(bad), Err(StoreError::Corrupt(_))), "{bad}");
    }
}

#[test]
fn status_mapping_follows_the_python_precedence() {
    assert_eq!(receipt_to_status(&receipt(Some(ReceiptErrorKind::IdempotencyMismatch))), 422);
    assert_eq!(receipt_to_status(&receipt(Some(ReceiptErrorKind::IdempotencyInFlight))), 409);
    assert_eq!(receipt_to_status(&receipt(Some(ReceiptErrorKind::InboxUnavailable))), 503);
    assert_eq!(receipt_to_status(&receipt(Some(ReceiptErrorKind::InterpreterStopped))), 409);
    assert_eq!(
        receipt_to_status(&receipt(Some(ReceiptErrorKind::Other("ActionError".into())))),
        STATUS_ERROR
    );
    assert_eq!(receipt_to_status(&receipt(None)), STATUS_OK);
    assert_eq!(
        receipt_to_status(&ReceiptJson { deferred: true, ..receipt(None) }),
        STATUS_ACCEPTED
    );
    assert_eq!(receipt_to_status(&ReceiptJson { denied: true, ..receipt(None) }), STATUS_CONFLICT);
    assert_eq!(receipt_to_status(&ReceiptJson { changed: true, ..receipt(None) }), STATUS_OK);
    assert_eq!(
        [STATUS_UNPROCESSABLE, STATUS_UNAVAILABLE],
        [422, 503],
        "named constants carry the documented numbers"
    );
}

#[test]
fn errors_outrank_flags_and_duplicate_changes_nothing() {
    let errored = ReceiptJson {
        deferred: true,
        denied: true,
        duplicate: true,
        ..receipt(Some(ReceiptErrorKind::IdempotencyMismatch))
    };
    assert_eq!(receipt_to_status(&errored), 422, "a duplicate of an errored delivery is an error");
    let deferred = ReceiptJson { deferred: true, denied: true, ..receipt(None) };
    assert_eq!(receipt_to_status(&deferred), 202, "deferred outranks denied");
    let duplicate = ReceiptJson { duplicate: true, changed: true, ..receipt(None) };
    assert_eq!(receipt_to_status(&duplicate), 200);
    assert_eq!(receipt_to_status(&duplicate.clone().into_duplicate()), 200);
}

const MACHINE: &str = r#"{"id":"m","initial":"a","states":{
    "a":{"on":{"GO":"b"}},
    "b":{"on":{"FIN":"c"}},
    "c":{"type":"final"}}}"#;

fn interpreter() -> SyncInterpreter {
    let machine = Machine::from_json(MACHINE).unwrap();
    let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
    interp.start().unwrap();
    interp
}

#[test]
fn engine_receipts_convert() {
    let mut interp = interpreter();
    let moved = interp.send("GO").unwrap();
    let json = ReceiptJson::from_receipt(&moved, ReceiptFlags::default(), None);
    assert_eq!(json.state_ids, ["m.b"]);
    assert!(json.changed && json.error.is_none());
    assert_eq!(receipt_to_status(&json), 200);

    let ignored = interp.send("NOPE").unwrap();
    let json = ReceiptJson::from_receipt(&ignored, ReceiptFlags::default(), None);
    assert!(!json.changed);
    assert_eq!(receipt_to_status(&json), 200, "an unhandled event is a clean no-op");

    let flags = ReceiptFlags { deferred: true, ..ReceiptFlags::default() };
    assert_eq!(receipt_to_status(&ReceiptJson::from_receipt(&ignored, flags, None)), 202);
}

#[test]
fn an_event_for_a_finished_machine_is_a_409() {
    let mut interp = interpreter();
    interp.send("GO").unwrap();
    interp.send("FIN").unwrap();
    let late = interp.send("GO").unwrap();
    let json = ReceiptJson::from_receipt(&late, ReceiptFlags::default(), None);
    assert_eq!(json.error.as_ref().unwrap().kind, ReceiptErrorKind::InterpreterStopped);
    assert_eq!(receipt_to_status(&json), 409);
    assert!(json.to_json().contains("InterpreterStoppedError"));
}

#[test]
fn explicit_errors_win_and_engine_errors_classify() {
    let mut interp = interpreter();
    let receipt = interp.send("GO").unwrap();
    let err =
        xstate_core::Error::RunawayChain { machine: "m".into(), limit: 1, event: "GO".into() };
    let json = ReceiptJson::from_receipt(
        &receipt,
        ReceiptFlags::default(),
        Some(ReceiptError::from_engine(&err)),
    );
    assert_eq!(
        json.error.as_ref().unwrap().kind,
        ReceiptErrorKind::Other("RunawayChainError".into())
    );
    assert_eq!(receipt_to_status(&json), 500);
    assert_eq!(
        json.to_value()["error"]["type"],
        json!("RunawayChainError"),
        "the JSON type carries the Python class name"
    );
}

#[test]
fn messages_can_be_omitted_and_the_default_keeps_them() {
    let r = ReceiptJson {
        error: Some(ReceiptError::new(ReceiptErrorKind::Other("ValueError".into()), "card=4111")),
        ..ReceiptJson::default()
    };
    assert_eq!(r.to_value()["error"]["message"], "card=4111");
    assert_eq!(r.to_value_with(true), r.to_value());
    assert_eq!(r.to_value_with(false)["error"], json!({"type": "ValueError", "message": ""}));
    assert!(r.to_json_with(false).contains(r#""message": """#));
    assert_eq!(ReceiptJson::default().to_value_with(false)["error"], json!(null));
}

#[test]
fn status_no_op_is_a_200_and_unresolved_target_is_a_500() {
    assert_eq!(STATUS_NO_OP, 200);
    assert_eq!(receipt_to_status(&receipt(None)), STATUS_NO_OP);
    let kind = ReceiptErrorKind::from_type_name("UnresolvedTargetError");
    assert_eq!(kind, ReceiptErrorKind::UnresolvedTarget);
    assert_eq!(kind.type_name(), "UnresolvedTargetError");
    assert_eq!(receipt_to_status(&receipt(Some(kind))), STATUS_ERROR);
}
