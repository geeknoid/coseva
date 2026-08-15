//! Downstream compile-pass/fail coverage for the `coseva` derive macros and
//! the exported `csv_format!` declaration macro.
//!
//! Unlike the unit tests in `coseva_macros_impl`, which inspect expansion
//! token text, and the entry-point tests, which assert only that a
//! `compile_error!` is produced, this suite compiles each fixture as an actual
//! downstream crate through the real proc-macro. That is the only way to prove
//! the diagnostic span and text, the generated trait bounds, crate-renaming
//! behaviour and privacy hygiene a caller actually sees.
//!
//! `pass/` fixtures must compile; `fail/` and `fail_rustc/` fixtures must fail
//! to compile and their diagnostics are snapshotted in the sibling `.stderr`
//! files. The two failure directories are split by *who owns the wording*, so a
//! future maintainer knows which snapshots to regenerate after a change:
//!
//! * `fail/` — diagnostics `coseva` itself emits: a derive `compile_error!` or
//!   a `csv_format!` const-evaluation panic. The wording is owned by this
//!   workspace, so these snapshots only change when we intentionally change a
//!   message.
//! * `fail_rustc/` — trait-bound, lifetime and type-mismatch diagnostics whose
//!   wording is owned by rustc. These are still snapshotted (trybuild requires
//!   a `.stderr` for every compile-fail fixture), but their text can drift when
//!   the compiler changes how it renders such errors.
//!
//! The committed snapshots are verified to match verbatim on both the MSRV
//! (1.95) and current stable toolchains; regenerate after an intentional
//! diagnostic change — or a future rustc wording change under `fail_rustc/` —
//! with `TRYBUILD=overwrite cargo test -p coseva_macros --test ui`.
//!
//! The manual implementation fixture characterizes the remaining
//! `StaticFormat` sealing limitation: an exported `csv_format!` macro expands
//! with downstream privileges, so every item it touches is also reachable by
//! hand-written downstream code. It remains a pass fixture until the sealing
//! design changes.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[test]
fn derive_and_format_macros_pass_and_fail_as_documented() {
    let t = trybuild::TestCases::new();

    // ── Compile-pass: every valid shape the derive and the format macro accept.
    t.pass("tests/ui/pass/*.rs");

    // ── Compile-fail: diagnostics coseva owns, snapshotted verbatim.
    t.compile_fail("tests/ui/fail/*.rs");

    // ── Compile-fail: rustc-owned trait/lifetime diagnostics, asserted to fail
    //    without a version-brittle snapshot.
    t.compile_fail("tests/ui/fail_rustc/*.rs");
}
