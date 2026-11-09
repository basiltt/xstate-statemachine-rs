#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Adversarial envelope coverage beyond the 24-case golden: CloudEvents 1.0 attribute rules,
//! extension-name and `traceparent` grammar, size/depth boundaries, malformed JSON and
//! Python-written envelopes read by Rust.

mod common;

use serde_json::{Value, json};
use xstate_eda::{Envelope, EnvelopeError, MAX_DATA_DEPTH};
use xstate_persistence::DEFAULT_MAX_SNAPSHOT_BYTES;

fn base() -> Value {
    json!({"specversion": "1.0", "id": "0190a0b4-1c2d-7e3f-8a4b-5c6d7e8f9a0b", "type": "t", "source": "s"})
}

#[allow(clippy::needless_pass_by_value)]
fn parse(raw: Value) -> Result<Envelope, EnvelopeError> {
    Envelope::from_dict(&raw)
}

// -- specversion -------------------------------------------------------------------------------

#[test]
fn specversion_must_be_the_string_1_0() {
    for bad in [json!(null), json!(1.0), json!(true), json!({"a": 1}), json!([])] {
        let mut raw = base();
        raw["specversion"] = bad.clone();
        let err = parse(raw).unwrap_err();
        assert!(err.to_string().starts_with("unsupported specversion "), "{bad}: {err}");
    }
    assert_eq!(parse(base()).unwrap().specversion, "1.0", "a correct specversion is accepted");
}

#[test]
fn an_id_type_or_source_that_is_falsy_is_replaced_or_refused_like_python() {
    // a falsy id (empty string, null, 0, false, [], {}) is replaced by a fresh id, as in Python
    for falsy in [json!(""), json!(null), json!(0), json!(false), json!([]), json!({})] {
        let mut raw = base();
        raw["id"] = falsy.clone();
        let env = parse(raw).unwrap();
        assert_eq!(env.id.len(), 36, "id replaced for {falsy}");
    }
    // type / source are required non-empty strings, never replaced
    for field in ["type", "source"] {
        for bad in [json!(""), json!(0), json!(false), json!([]), json!({})] {
            let mut raw = base();
            raw[field] = bad;
            let err = parse(raw).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("envelope attribute '{field}' must be a non-empty string"),
                "{err}"
            );
        }
    }
}

#[test]
fn optional_string_attributes_accept_any_string_but_no_other_type() {
    for field in [
        "subject",
        "time",
        "datacontenttype",
        "correlationid",
        "causationid",
        "machineid",
        "machineversion",
    ] {
        for bad in [json!(7), json!(1.5), json!(true), json!(["x"]), json!({"a": 1})] {
            let mut raw = base();
            raw[field] = bad;
            let err = parse(raw).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("envelope attribute '{field}' must be a non-empty string"),
                "{err}"
            );
        }
        // an empty optional string is allowed (subject "" survives, unlike None)
        let mut raw = base();
        raw[field] = json!("");
        assert!(parse(raw).is_ok(), "{field} accepts an empty string");
    }
    // null optional attributes are omitted
    let mut raw = base();
    raw["subject"] = json!(null);
    assert!(parse(raw).unwrap().subject.is_none());
}

// -- extension-name grammar --------------------------------------------------------------------

#[test]
fn extension_names_obey_the_cloud_events_grammar() {
    let ok = |name: &str| {
        let mut raw = base();
        raw[name] = json!("v");
        parse(raw).is_ok()
    };
    assert!(ok("a"));
    assert!(ok("a0b1c2d3e4f5g6h7i8j9")); // exactly 20
    assert!(ok("0"));
    let mut raw = base();
    raw["a0b1c2d3e4f5g6h7i8j9x"] = json!("v"); // 21
    assert!(parse(raw).is_err());
    for bad in ["A", "a-b", "a_b", "a.b", "a b", "aé", ""] {
        let mut raw = base();
        raw[bad] = json!("v");
        let err = parse(raw).unwrap_err();
        assert!(err.to_string().starts_with("invalid CloudEvents extension name "), "{bad}: {err}");
    }
}

#[test]
fn extension_values_are_string_integer_or_boolean_only() {
    for bad in [json!(1.5), json!(null), json!([1]), json!({"a": 1})] {
        let mut raw = base();
        raw["tenant"] = bad.clone();
        let err = parse(raw).unwrap_err();
        assert_eq!(err.to_string(), "extension 'tenant' must be a string, integer or boolean");
    }
    for good in [json!("s"), json!(0), json!(-3), json!(true), json!(false)] {
        let mut raw = base();
        raw["tenant"] = good.clone();
        assert!(parse(raw).is_ok(), "{good} is a valid extension value");
    }
}

// -- traceparent grammar -----------------------------------------------------------------------

#[test]
fn traceparent_grammar_is_exactly_the_w3c_shape() {
    let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let mut accept = vec![valid.to_owned()];
    accept.push("fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00".to_owned());
    accept.push("0f-00000000000000000000000000000001-0000000000000001-ff".to_owned());
    for tp in accept {
        let mut raw = base();
        raw["traceparent"] = json!(tp);
        assert!(parse(raw).is_ok(), "accepted {tp}");
    }
    let mut refuse: Vec<String> = vec![
        valid.replacen("00", "ff", 1), // version ff
        valid.to_uppercase(),          // uppercase hex
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01".into(), // all-zero trace
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01".into(), // all-zero span
        format!("{valid}-extra"),      // five parts
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7".into(), // three parts
        "0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(), // one-hex version
        "00-4bf92f3577b34da6a3ce929d0e0e4736g-00f067aa0ba902b7-01".into(), // non-hex trace
    ];
    refuse.push("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0".into()); // one-hex flags
    for tp in refuse {
        let mut raw = base();
        raw["traceparent"] = json!(tp);
        assert_eq!(
            parse(raw).unwrap_err().to_string(),
            "invalid W3C traceparent extension",
            "refused {tp}"
        );
    }
}

// -- size and depth boundaries -----------------------------------------------------------------

#[test]
fn size_caps_are_enforced_on_the_exact_boundary() {
    let text = r#"{"specversion":"1.0","id":"i","type":"t","source":"s"}"#;
    let bytes = text.as_bytes();
    assert!(Envelope::from_json_bytes(bytes, bytes.len()).is_ok());
    let err = Envelope::from_json_bytes(bytes, bytes.len() - 1).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("envelope is {} bytes; the limit is {}", bytes.len(), bytes.len() - 1)
    );
    // to_json refuses a payload one byte over the cap, accepts exactly the cap
    let env = Envelope::builder("t", "s").data(json!({"x": "y"})).build().unwrap();
    let exact = env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap();
    assert!(env.to_json(exact.len()).is_ok());
    let err = env.to_json(exact.len() - 1).unwrap_err();
    assert!(matches!(err, EnvelopeError::TooLarge(_)), "{err}");
}

#[test]
fn data_depth_is_enforced_at_the_64_level_boundary() {
    let mut v: Value = json!(1);
    for _ in 0..MAX_DATA_DEPTH {
        v = Value::Array(vec![v]);
    }
    let mut raw = base();
    raw["data"] = v.clone();
    assert!(parse(raw).is_ok(), "exactly {MAX_DATA_DEPTH} levels is fine");

    let mut raw = base();
    raw["data"] = Value::Array(vec![v]);
    let err = parse(raw).unwrap_err();
    assert_eq!(err.to_string(), format!("envelope data is nested deeper than {MAX_DATA_DEPTH}"));
}

#[test]
fn depth_check_walks_both_arrays_and_objects_and_stops_at_leaves() {
    let mut raw = base();
    raw["data"] = json!({"a": {"b": {"c": [1, 2, 3]}}});
    assert!(parse(raw).is_ok(), "a 3-level object is well under the cap");
}

// -- malformed JSON and non-finite floats ------------------------------------------------------

#[test]
fn malformed_json_is_corrupt_never_a_panic() {
    for bad in [
        "",
        "not json",
        "{",
        "{\"id\":",
        "NaN",
        "Infinity",
        "-Infinity",
        "{\"a\": NaN}",
        "{\"a\": Infinity}",
        "{\"a\": -Infinity}",
        "{\"a\": 1,}",
        "null",
        "[]",
        "42",
        "\"str\"",
    ] {
        let err = Envelope::from_json(bad, 10_000).unwrap_err();
        assert!(matches!(err, EnvelopeError::Corrupt(_)), "{bad:?}: {err:?}");
    }
}

#[test]
fn non_finite_floats_become_null_not_invalid_tokens() {
    // serde_json cannot hold NaN/Infinity: constructing a Value with them yields null
    let env = Envelope::builder("t", "s")
        .data(json!({"nan": f64::NAN, "inf": f64::INFINITY, "n": 1}))
        .build()
        .unwrap();
    let text = env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap();
    assert!(!text.contains("NaN") && !text.contains("Infinity"), "{text}");
    let round = Envelope::from_json(&text, DEFAULT_MAX_SNAPSHOT_BYTES).unwrap();
    assert_eq!(round.data, json!({"nan": null, "inf": null, "n": 1}));
}

// -- from_transition subject / correlation defaults --------------------------------------------

#[test]
fn from_transition_subject_defaults_to_instance_key_then_machine_id() {
    let machine = common::counter();
    for (instance_key, subject, expected) in [
        (None, None, "counter"),
        (Some("o-1"), None, "o-1"),
        (None, Some("biz"), "biz"),
        (Some("o-1"), Some("biz"), "biz"),
    ] {
        let origin =
            xstate_eda::TransitionOrigin { machine: &machine, instance_key, subject, source: None };
        let env = Envelope::from_transition(&origin, "order.paid", json!({"n": 1}), None).unwrap();
        assert_eq!(env.subject.as_deref(), Some(expected));
        assert_eq!(env.source, "xsm/counter");
        assert_eq!(env.machineid.as_deref(), Some("counter"));
        assert_eq!(env.correlationid.as_deref(), Some(env.id.as_str()), "self-correlated");
        assert!(env.causationid.is_none());
    }
}

#[test]
fn from_transition_inherits_correlation_and_propagates_traceparent() {
    let machine = common::counter();
    let cause = Envelope::new("xsm.counter.ADD")
        .subject("o-1")
        .correlationid("corr-9")
        .extension("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .build()
        .unwrap();
    let origin = xstate_eda::TransitionOrigin::new(&machine);
    let env = Envelope::from_transition(&origin, "order.paid", json!({}), Some(&cause)).unwrap();
    assert_eq!(env.causationid.as_deref(), Some(cause.id.as_str()));
    assert_eq!(env.correlationid.as_deref(), Some("corr-9"));
    assert_eq!(env.subject.as_deref(), Some("o-1"), "cause subject is the default");
    assert_eq!(env.extensions["traceparent"], cause.extensions["traceparent"]);
}

// -- Python-written envelopes ------------------------------------------------------------------

const PY_WRITE_ENVELOPES: &str = r#"
import sys, json
from xstate_statemachine.eda import Envelope, EnvelopeCorruptError
cases = []
def emit(name, **kw):
    try:
        e = Envelope.new(type="t", **kw)
        cases.append([name, "ok", e.to_json(), None])
    except EnvelopeCorruptError as ex:
        cases.append([name, "corrupt", None, str(ex)])
emit("large_int", extensions={"tenant": 123456789012345678901234567890})
emit("bool_ext", extensions={"flag": True, "off": False, "neg": -1})
emit("unicode", subject="日本-😀", data={"note": "café"})
emit("traceparent", extensions={"traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"})
emit("bad_ext_name", extensions={"Tenant": "x"})
print(json.dumps(cases))
"#;

#[test]
fn rust_reads_python_written_envelopes() {
    let Some(py) = common::python() else { return };
    let out: Value =
        serde_json::from_str(&common::run_python(&py, PY_WRITE_ENVELOPES, &[])).unwrap();
    for case in out.as_array().unwrap() {
        let (name, status) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
        match (name, status) {
            // Python's json keeps this as an arbitrary-precision int; Rust's serde_json cannot
            // represent it, so `from_json` rejects the whole envelope. A real compatibility
            // gap between the two runtimes, not a parse we silently accept.
            ("large_int", "ok") => {
                let text = case[2].as_str().unwrap();
                assert!(matches!(
                    Envelope::from_json(text, 1 << 20),
                    Err(EnvelopeError::Corrupt(_))
                ));
            }
            (_, "ok") => {
                let env = Envelope::from_json(case[2].as_str().unwrap(), 1 << 20).unwrap();
                assert_eq!(env.ty, "t", "{name}");
                let back = Envelope::from_json(
                    &env.to_json(DEFAULT_MAX_SNAPSHOT_BYTES).unwrap(),
                    DEFAULT_MAX_SNAPSHOT_BYTES,
                )
                .unwrap();
                assert_eq!(back, env, "{name}");
            }
            (_, "corrupt") => {
                assert!(case[3].is_string(), "{name}: Python recorded a message");
            }
            other => panic!("{other:?}: unknown status"),
        }
    }
}

#[test]
fn rust_rejects_what_python_rejects_for_extension_names() {
    // a Python envelope with an uppercase extension name is refused by both runtimes
    let Some(py) = common::python() else { return };
    let script = r#"
import sys
from xstate_statemachine.eda import Envelope, EnvelopeCorruptError
try:
    Envelope.new(type="t", extensions={"Tenant": "x"})
    print("accepted")
except EnvelopeCorruptError as ex:
    print(str(ex))
"#;
    let msg = common::run_python(&py, script, &[]);
    assert!(msg.contains("invalid CloudEvents extension name"), "{msg}");
    let mut raw = base();
    raw["Tenant"] = json!("x");
    let err = parse(raw).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid CloudEvents extension name 'Tenant' (lower-case letters/digits, at most 20)"
    );
}
