//! The parser must never panic on arbitrary JSON, whatever the shape: every input either
//! produces a configuration or a typed error. Documented as the parser fuzz target in
//! `docs/HANDOVER-QA.md` §6.2; this is the in-process property that runs in CI.

use proptest::prelude::*;
use serde_json::Value;

use super::*;

/// A bounded JSON value: leaves are scalars, composite values nest to a few levels.
fn json_value(depth: u32) -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        ".{0,16}".prop_map(Value::String),
    ];
    leaf.prop_recursive(depth, 24, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec(("[a-zA-Z_$][a-zA-Z0-9_$.-]{0,8}", inner), 0..4)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// `MachineConfig::from_value`, `Machine::from_value` and both text entry points accept or
    /// refuse arbitrary JSON without panicking (proptest turns any panic into a failure).
    #[test]
    fn arbitrary_json_never_panics(value in json_value(4)) {
        let _ = MachineConfig::from_value(&value);
        let _ = crate::machine::Machine::from_value(&value);
        let text = serde_json::to_string(&value).expect("a Value always serialises");
        let _ = MachineConfig::from_json(&text);
        let _ = MachineConfig::from_json(&text);
    }

    /// Arbitrary text never reaches the parser: `serde_json` either refuses it as `Error::Json`
    /// or hands over a value, and neither path panics.
    #[test]
    fn arbitrary_text_never_panics(text in ".{0,256}") {
        let _ = crate::machine::Machine::from_json(&text);
        let _ = MachineConfig::from_json(&text);
        let _ = crate::snapshot::Snapshot::from_json(&text);
    }
}
