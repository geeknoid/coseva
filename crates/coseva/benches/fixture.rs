//! The corpus and checksum every record benchmark shares.
//!
//! Three benchmark files measure the same bytes through different record
//! shapes, and a comparison between their tables is only meaningful if the
//! input is identical down to the byte. Keeping the corpus here rather than
//! copying it into each file is what makes that true by construction instead
//! of by inspection.
//!
//! Included with `#[path]` rather than reached through the library, because a
//! benchmark file is its own crate and `coseva` must not carry fixtures in its
//! public surface. `autobenches = false` in `Cargo.toml` keeps Cargo from
//! mistaking this for a benchmark target of its own.
//!
//! # Numbers are only comparable within one build of one file
//!
//! Sharing the corpus makes two tables comparable in their *input*. It does not
//! make them comparable in their *numbers*, and the difference has been
//! measured rather than assumed.
//!
//! A throwaway suite parsed 1000 rows of [`ROWS_1000`] into a `ByteRecord` at
//! 675,270 instructions. Adding a second, entirely unrelated benchmark function
//! to the same file — different corpus, different statics, never called by the
//! first — moved that same unchanged case to 833,590, a 23% rise. Deleting the
//! addition returned it to 675,270 exactly. Along the way, three cheaper
//! explanations were tested and ruled out: record capacity (52 against 57 bytes
//! is instruction-identical), one function serving several corpora, and input
//! alignment (offsets of 0, 1, 8, 16 and 32 bytes span 0.04%).
//!
//! What remains is that each benchmark file is a separate binary, and the
//! optimizer's inlining decisions inside a measured loop depend on the rest of
//! that binary. This is why `dialects` measures 672 instructions per record
//! where `quoted` measures 791 for identical work.
//!
//! Two rules follow, and both are stricter than they look:
//!
//! - **Compare rows within one table, never across files.** A suite that needs
//!   a reference point must measure it in its own file, as `dialects` does.
//! - **A published table belongs to the commit that produced it.** Adding a
//!   case to a file can move every other number in it, so the whole table must
//!   be re-measured after any edit — not just the row that changed.

/// One unquoted six-field record, terminated by a newline.
pub(crate) const ROW: &[u8] = b"Boston,Massachusetts,4500000,42.3601,-71.0589,true\n";

/// The width of [`ROW`], which every corpus is an exact multiple of.
pub(crate) const ROW_LEN: usize = ROW.len();

/// The sum of the six field lengths in [`ROW`], excluding its delimiters.
pub(crate) const FIELD_BYTES: u64 = 6 + 13 + 7 + 7 + 8 + 4;

/// The number of fields in [`ROW`].
pub(crate) const FIELDS: usize = 6;

/// The read buffer every buffered front end is pinned to.
///
/// This is the `csv` crate's own default, set explicitly on both sides so the
/// comparison cannot quietly become a comparison of default buffer sizes.
pub(crate) const BUFFER: usize = 8 * 1024;

/// Build a corpus of `N / ROW_LEN` copies of [`ROW`] at compile time.
///
/// Generating the corpus in a `const fn` keeps every fixture a `static`, so no
/// case pays for building or allocating its input, in the measured region or
/// out of it.
pub(crate) const fn corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        out[index] = ROW[index % ROW_LEN];
        index += 1;
    }
    out
}

static BUF_1: [u8; ROW_LEN] = corpus();
static BUF_10: [u8; ROW_LEN * 10] = corpus();
static BUF_100: [u8; ROW_LEN * 100] = corpus();
static BUF_1000: [u8; ROW_LEN * 1000] = corpus();

pub(crate) static ROWS_1: &[u8] = &BUF_1;
pub(crate) static ROWS_10: &[u8] = &BUF_10;
pub(crate) static ROWS_100: &[u8] = &BUF_100;
pub(crate) static ROWS_1000: &[u8] = &BUF_1000;

/// Assert the case walked every field of every record in `input`.
///
/// Asserted rather than assumed, so a case cannot quietly drift into parsing
/// something else and still look comparable.
pub(crate) fn check(total: u64, input: &[u8]) -> u64 {
    let expected = (input.len() / ROW_LEN) as u64 * FIELD_BYTES;
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    total
}

/// Drop a value outside the measured region.
pub(crate) fn drop_it<T>(value: T) {
    drop(value);
}
