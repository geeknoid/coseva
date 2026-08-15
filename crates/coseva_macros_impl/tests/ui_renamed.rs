//! Renamed-crate compile-pass coverage for the `coseva` derive macros.
//!
//! The derive emits absolute `::coseva::…` paths, so a downstream crate that
//! renames the `coseva` package must be able to bind that name back with
//! `extern crate … as coseva;`. This can only be exercised from a crate whose
//! sole `coseva` dependency is the alias, because Cargo rejects depending on
//! one package under two names; `coseva_macros_impl` is that crate. See the
//! main UI suite in `coseva_macros/tests/ui.rs` for every other scenario.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[test]
fn derive_resolves_through_a_renamed_coseva() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui_renamed/pass/*.rs");
}
