#![no_std]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![expect(
    missing_docs,
    reason = "this private crate exposes implementation details only to coseva"
)]
#![expect(
    clippy::inline_always,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::multiple_unsafe_ops_per_block,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::question_mark,
    clippy::semicolon_outside_block,
    clippy::too_many_lines,
    clippy::unnecessary_semicolon,
    reason = "private hot-path internals retain the code shape and API annotations of their \
              former crate-private definitions"
)]
//! Private low-level operations for `coseva`.
//!
//! Every public function in this crate is safe to call. Architecture-specific
//! intrinsics and carefully bounded raw-memory operations stay here so the
//! user-facing crates can forbid unsafe code.

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod bytes;
pub mod record;
pub mod search;
pub mod span;
pub mod storage;
