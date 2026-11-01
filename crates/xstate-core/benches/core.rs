//! Placeholder benchmark wiring for the Phase 1 perf gate (see `benches/README.md` at the
//! workspace root). It measures the cost of parsing a small machine configuration as JSON so
//! the criterion harness, the `budgets.json` gate and CI are exercised before the engine
//! exists.

#![allow(missing_docs)] // criterion macros generate undocumented items

use criterion::{Criterion, black_box, criterion_group, criterion_main};

const TOGGLE: &str = r#"{"id":"toggle","initial":"inactive","states":{
  "inactive":{"on":{"TOGGLE":"active"}},"active":{"on":{"TOGGLE":"inactive"}}}}"#;

fn parse_toggle(c: &mut Criterion) {
    c.bench_function("parse_json_toggle", |b| {
        b.iter(|| {
            let v: serde_json::Value =
                serde_json::from_str(black_box(TOGGLE)).unwrap_or(serde_json::Value::Null);
            black_box(v)
        });
    });
}

criterion_group!(benches, parse_toggle);
criterion_main!(benches);
