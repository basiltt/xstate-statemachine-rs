use super::*;
use crate::config::GuardConfig;

fn parse(text: &str) -> MachineConfig {
    MachineConfig::from_json(text).expect("parses")
}

fn parse_err(text: &str) -> String {
    MachineConfig::from_json(text).expect_err("must be refused").to_string()
}

#[test]
fn shorthands_expand() {
    let cfg = parse(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":"b","STAY":null,"X":[{"target":"b","cond":"g"},"a"]}},"b":{}}}"#,
    );
    let a = &cfg.root.states[0];
    assert_eq!(a.on.len(), 3);
    assert_eq!(a.on[0].1[0].targets, vec!["b"]);
    assert!(a.on[1].1[0].forbidden);
    assert_eq!(a.on[2].1.len(), 2);
    assert_eq!(
        a.on[2].1[0].guard,
        Some(GuardConfig::Named { name: "g".into(), params: Value::Null })
    );
}

#[test]
fn inferred_initial_and_kinds() {
    let cfg = parse(
        r#"{"id":"m","initial":"p","states":{"p":{"type":"parallel","states":{"r":{"states":{"only":{}}}}},"h":{"type":"history","history":"deep"},"f":{"type":"final"}}}"#,
    );
    assert_eq!(cfg.root.states[0].kind, StateKind::Parallel);
    assert_eq!(cfg.root.states[0].states[0].initial.as_deref(), Some("only"));
    assert_eq!(cfg.root.states[1].kind, StateKind::History { deep: true, target: None });
    assert_eq!(cfg.root.states[2].kind, StateKind::Final);
}

#[test]
fn unknown_keys_warn_or_fail_with_paths() {
    let cfg = parse(r#"{"id":"m","initial":"a","states":{"a":{"entri":"x"}}}"#);
    assert_eq!(cfg.warnings.len(), 1);
    assert_eq!(cfg.warnings[0].path, "states.a.entri");
    assert!(cfg.warnings[0].message.contains("entry"));
    let err = parse_err(
        r#"{"id":"m","strictConfig":true,"initial":"a","states":{"a":{"on":{"GO":[{"target":"b","gard":"x"}]}},"b":{}}}"#,
    );
    assert!(err.contains("states.a.on.GO[0].gard"), "{err}");
}

#[test]
fn flat_built_in_params_are_refused_with_a_hint() {
    let err = parse_err(
        r#"{"id":"m","initial":"a","states":{"a":{"entry":{"type":"raise","event":"X"}}}}"#,
    );
    assert!(err.contains("nested under `params`"), "{err}");
    assert!(err.contains("State 'm.a' entry"), "{err}");
}

#[test]
fn state_types_are_validated() {
    for bad in ["Final", "paralel", "leaf"] {
        let err = parse_err(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"type":"{bad}"}}}}}}"#
        ));
        assert!(err.contains("State 'm.a' has an invalid 'type'"), "{err}");
    }
    for container in ["compound", "parallel"] {
        let err = parse_err(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"type":"{container}"}}}}}}"#
        ));
        assert!(err.contains("has no child 'states'"), "{err}");
    }
    let err =
        parse_err(r#"{"id":"m","initial":"a","states":{"a":{"type":"parallel","states":{}}}}"#);
    assert!(err.contains("has no child 'states'"), "{err}");
    for leaf in ["atomic", "final", "history"] {
        let cfg = parse(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"type":"{leaf}","states":{{"x":{{}}}}}}}}}}"#
        ));
        assert_eq!(cfg.root.states[0].kind, StateKind::Compound, "{leaf}");
        assert!(cfg.warnings.iter().any(|w| w.message.contains("Building it as 'compound'")));
    }
}

#[test]
fn history_value_and_default_target_are_validated() {
    let err = parse_err(
        r#"{"id":"m","initial":"a","states":{"a":{},"h":{"type":"history","history":"dep"}}}"#,
    );
    assert!(err.contains("'history' must be"), "{err}");
    let err = parse_err(
        r#"{"id":"m","initial":"a","states":{"a":{},"h":{"type":"history","target":""}}}"#,
    );
    assert!(err.contains("non-empty state name"), "{err}");
}

#[test]
fn boolean_flags_accept_only_booleans_or_null() {
    for (key, value) in
        [("strict", r#""false""#), ("strictTargets", "1"), ("strictConfig", r#""yes""#)]
    {
        let err = parse_err(&format!(
            r#"{{"id":"m","{key}":{value},"initial":"a","states":{{"a":{{}}}}}}"#
        ));
        assert!(err.contains("must be true or false"), "{err}");
    }
    let cfg = parse(
        r#"{"id":"m","strict":null,"strictTargets":null,"strictConfig":null,"initial":"a","states":{"a":{}}}"#,
    );
    assert!(!cfg.policies.strict && cfg.policies.strict_targets);
    for flag in ["reenter", "internal"] {
        let err = parse_err(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"on":{{"GO":{{"target":"a","{flag}":"false"}}}}}}}}}}"#
        ));
        assert!(err.contains("Transition 'GO' on state 'm.a'"), "{err}");
    }
    let cfg = parse(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":{"target":"a","reenter":null,"internal":false}}}}}"#,
    );
    assert!(cfg.root.states[0].on[0].1[0].reenter);
}

#[test]
fn errors_name_the_state_and_key() {
    let err = parse_err(
        r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":{"target":"a","guard":5}}}}}"#,
    );
    assert!(err.contains("Transition 'GO' on state 'm.a': invalid 'guard'"), "{err}");
    let err = parse_err(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":[5]}}}}"#);
    assert!(err.contains("State 'm.a' on 'GO': invalid transition item"), "{err}");
    let err = parse_err(r#"{"id":"m","initial":"a","states":{"a":{"exit":[{"params":{}}]}}}"#);
    assert!(err.contains("State 'm.a' exit") && err.contains("`type`"), "{err}");
}

#[test]
fn action_objects_are_checked() {
    let err =
        parse_err(r#"{"id":"m","initial":"a","states":{"a":{"entry":{"type":"x","params":5}}}}"#);
    assert!(err.contains("'params' must be an object"), "{err}");
    let err =
        parse_err(r#"{"id":"m","initial":"a","states":{"a":{"entry":{"type":"x","params":[1]}}}}"#);
    assert!(err.contains("'params' must be an object"), "{err}");
    for falsy in ["0", "false", r#""""#] {
        let err = parse_err(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"entry":{falsy}}}}}}}"#
        ));
        assert!(err.contains("State 'm.a' entry"), "{err}");
    }
    // Unknown keys: a warning by default, an error under strictConfig.
    let lenient = r#"{"id":"m","initial":"a","states":{"a":{"entry":[{"type":"x","parms":{}},{"type":"raise","params":{"event":"E","delya":5}}],"on":{"E":{"guard":{"type":"g","param":1}}}}}}"#;
    let cfg = parse(lenient);
    let found: Vec<&str> = cfg.warnings.iter().map(|w| w.path.as_str()).collect();
    assert_eq!(
        found,
        ["states.a.on.E.guard.param", "states.a.entry[0].parms", "states.a.entry[1].params.delya"]
    );
    let strict = lenient.replacen(r#""id":"m","#, r#""id":"m","strictConfig":true,"#, 1);
    assert!(MachineConfig::from_json(&strict).is_err());
}

#[test]
fn invoke_fields_are_checked() {
    for (field, value) in [("id", r#""""#), ("systemId", r#""""#), ("description", "5")] {
        let err = parse_err(&format!(
            r#"{{"id":"m","initial":"a","states":{{"a":{{"invoke":{{"src":"s","{field}":{value}}}}}}}}}"#
        ));
        assert!(err.contains(&format!("'{field}' must be")), "{err}");
    }
}

#[test]
fn empty_targets_and_child_keys_are_refused() {
    let err = parse_err(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":""}}}}"#);
    assert!(err.contains("empty-string `target`"), "{err}");
    let err = parse_err(r#"{"id":"m","initial":"a","states":{"a":{"on":{"GO":{"target":[""]}}}}}"#);
    assert!(err.contains("empty-string `target`"), "{err}");
    let err = parse_err(r#"{"id":"m","initial":"a","states":{"a":{},"":{}}}"#);
    assert!(err.contains("non-empty"), "{err}");
}

#[test]
fn root_numbers_are_validated() {
    for bad in ["0", "-3", "2.9", "true", r#""5""#, "null"] {
        let err = parse_err(&format!(
            r#"{{"id":"m","maxIterations":{bad},"initial":"a","states":{{"a":{{}}}}}}"#
        ));
        assert!(err.contains("whole number >= 1"), "{bad}: {err}");
    }
    let cfg = parse(r#"{"id":"m","maxIterations":5.0,"initial":"a","states":{"a":{}}}"#);
    assert_eq!(cfg.policies.max_iterations, 5);
    for bad in ["-1", r#""nan""#, r#""inf""#, "true"] {
        let err = parse_err(&format!(
            r#"{{"id":"m","spawnBlockingTimeout":{bad},"initial":"a","states":{{"a":{{}}}}}}"#
        ));
        assert!(err.contains("finite number >= 0"), "{bad}: {err}");
    }
}

#[test]
fn initial_must_name_a_child_with_a_hint() {
    let err = parse_err(r#"{"id":"m","initial":"idel","states":{"idle":{},"busy":{}}}"#);
    assert!(err.contains("'initial' 'idel' is not one of its child states"), "{err}");
    assert!(err.contains("Did you mean 'idle'?"), "{err}");
}
