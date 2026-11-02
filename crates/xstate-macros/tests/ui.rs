//! Compile-fail cases: macro-level mistakes must be compile errors pointing at the culprit.

#[test]
fn ui() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
