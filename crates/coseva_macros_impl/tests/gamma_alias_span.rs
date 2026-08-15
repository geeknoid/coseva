//! UI coverage for diagnostics whose source span is behaviorally significant.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[test]
fn skip_conflict_points_to_the_first_alias() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/gamma_alias_span/fail/*.rs");
}
