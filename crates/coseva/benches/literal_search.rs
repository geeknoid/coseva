//! Whether the literal search's anchor byte determines how fast an absent
//! needle is proven absent.
//!
//! # The regression this guards against
//!
//! Before the rare-byte anchor, `find_literal` (backing both raw-input pushdown and
//! [`Predicate::contains`]) anchored its SIMD scan on the needle's *first*
//! byte and confirmed each hit with a full compare. An audit probe measured
//! that construction proving a ten-byte needle absent from one mebibyte of
//! `'a'`: 99.6M instructions when the needle's first byte was `'a'` (so
//! every position in the haystack looked like a candidate, each confirmed
//! almost to the end before failing), against 1.57M when the first byte was
//! rare (so the SIMD scan itself found no candidates at all) — a 63×
//! spread that depended entirely on an accident of which byte the needle
//! happened to start with.
//!
//! `find_literal` now runs the two-way string-matching algorithm (Crochemore
//! & Perrin, 1991) with its SIMD anchor on the needle's *last* byte instead
//! of the first, chosen once from the needle's own critical factorization
//! rather than always being position zero. The absent-anchor group pins that
//! fix down: two needles that are identical except for whether their *leading*
//! byte is common or rare, but share the same rare *trailing* byte, must now
//! cost the same, because neither needle's cost depends on its leading byte.
//!
//! # The two absent-anchor cases
//!
//! Both are ten bytes, absent from the haystack, and end in the same rare
//! byte (`'z'`), so [`RARE_LEADING`] and [`COMMON_LEADING`] differ only in
//! their first byte:
//!
//! - [`COMMON_LEADING`]: `b"aaaaaaaaaz"` — leading byte common in the haystack.
//! - [`RARE_LEADING`]: `b"zaaaaaaaaz"` — leading byte rare in the haystack.
//!
//! # Results
//!
//! Callgrind instruction counts for one [`Predicate::contains`] search of a
//! 1 MiB haystack.
//!
//! | Case             | Instructions | vs `rare_leading` |
//! |------------------|--------------|--------------------|
//! | `rare_leading`   |      263,014 |                    |
//! | `common_leading` |      263,053 |              1.00× |
//!
//! # What the numbers say
//!
//! The two cases are indistinguishable — 39 instructions apart out of
//! 263,000, or 0.015%, which is noise, not signal. Nothing about the
//! search's cost depends on the needle's leading byte, because the leading
//! byte is not what selects candidates. Both cases
//! anchor on the shared trailing `'z'` — the two-way engine's SIMD scan for
//! that one byte across the whole haystack finds nothing, and neither case
//! ever reaches the engine's confirmation step at all.
//!
//! This suite is the standing regression guard that the two cases stay
//! indistinguishable: a search whose cost depends on the needle's leading
//! byte would separate them again.
//!
//! The `positioned_search` group covers the branches that relationship cannot:
//! an exact match at byte 64, an exact match 64 bytes from the end of the same
//! 1 MiB input, and an absent periodic near-hit whose common final byte forces
//! repeated candidate confirmation. Teardown checks the exact leftmost offset
//! (or exact absence), while each path has its own instruction baseline rather
//! than an invalid ratio between unlike selectivities.
//!
//! [`Predicate::contains`]: coseva::Predicate::contains

#![expect(
    missing_docs,
    reason = "benchmark macros are private and this file's items are documented individually"
)]

use std::hint::black_box;

use coseva::Predicate;
use coseva_unsafe::search::find_literal;
use gungraun::prelude::*;

/// A haystack that contains neither case's needle anywhere, so both searches
/// must scan to the end to prove absence rather than stopping at a match.
static HAYSTACK: [u8; 1024 * 1024] = [b'a'; 1024 * 1024];

const HAYSTACK_LEN: usize = 1024 * 1024;
const MATCH_NEEDLE: &[u8] = b"aaaaaaaaz";
const EARLY_OFFSET: usize = 64;
const LATE_OFFSET: usize = HAYSTACK_LEN - MATCH_NEEDLE.len() - 64;
const NEAR_HIT_NEEDLE: &[u8] = b"aaaaaaaaabaaaaaaaaa";

/// The case whose leading byte (`'a'`) is common in [`HAYSTACK`], so every
/// position looks like a candidate under a first-byte
/// anchor. Ends in the same rare byte as [`RARE_LEADING`].
const COMMON_LEADING: &[u8] = b"aaaaaaaaaz";

/// The case whose leading byte is the same rare byte the needle already ends
/// in. Everything after the first byte,
/// including the length and the trailing `'z'`, is identical to
/// [`COMMON_LEADING`].
const RARE_LEADING: &[u8] = b"zaaaaaaaaz";

fn check(found: bool) -> bool {
    assert!(!found, "benchmark needle must be absent from the haystack");
    found
}

fn predicate_state(needle: &'static [u8]) -> Predicate {
    Predicate::contains(0, needle)
}

fn drop_it<T>(value: T) {
    drop(value);
}

#[derive(Clone, Copy)]
enum SearchFixture {
    PresentEarly,
    PresentLate,
    NearHitAbsent,
}

struct SearchState {
    haystack: Vec<u8>,
    needle: &'static [u8],
    expected: Option<usize>,
}

fn search_state(fixture: SearchFixture) -> SearchState {
    let mut haystack = vec![b'a'; HAYSTACK_LEN];
    let (needle, expected) = match fixture {
        SearchFixture::PresentEarly => {
            haystack[EARLY_OFFSET..EARLY_OFFSET + MATCH_NEEDLE.len()].copy_from_slice(MATCH_NEEDLE);
            (MATCH_NEEDLE, Some(EARLY_OFFSET))
        }
        SearchFixture::PresentLate => {
            haystack[LATE_OFFSET..LATE_OFFSET + MATCH_NEEDLE.len()].copy_from_slice(MATCH_NEEDLE);
            (MATCH_NEEDLE, Some(LATE_OFFSET))
        }
        SearchFixture::NearHitAbsent => (NEAR_HIT_NEEDLE, None),
    };
    SearchState {
        haystack,
        needle,
        expected,
    }
}

fn check_position((found, state): (Option<usize>, SearchState)) {
    assert_eq!(
        found, state.expected,
        "literal search must return the exact leftmost benchmark position"
    );
    if let Some(offset) = found {
        assert_eq!(
            &state.haystack[offset..offset + state.needle.len()],
            state.needle,
            "reported benchmark position must contain the complete needle"
        );
    }
}

#[library_benchmark]
#[bench::rare_leading(args = (RARE_LEADING), setup = predicate_state, teardown = drop_it)]
#[bench::common_leading(args = (COMMON_LEADING), setup = predicate_state, teardown = drop_it)]
fn absent_needle(predicate: Predicate) -> (bool, Predicate) {
    let found = predicate.matches_field(Some(black_box(HAYSTACK.as_slice())));
    (black_box(check(found)), predicate)
}

#[library_benchmark]
#[bench::present_early(
    args = (SearchFixture::PresentEarly),
    setup = search_state,
    teardown = check_position
)]
#[bench::present_late(
    args = (SearchFixture::PresentLate),
    setup = search_state,
    teardown = check_position
)]
#[bench::near_hit_absent(
    args = (SearchFixture::NearHitAbsent),
    setup = search_state,
    teardown = check_position
)]
fn positioned_search(state: SearchState) -> (Option<usize>, SearchState) {
    let found = find_literal(
        black_box(state.needle),
        black_box(state.haystack.as_slice()),
    );
    (black_box(found), state)
}

library_benchmark_group!(
    name = literal_search;
    benchmarks = absent_needle, positioned_search
);

main!(library_benchmark_groups = literal_search);
