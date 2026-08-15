//! The quoting predicate's two scans, at widths either side of the threshold.
//!
//! Emission asks `needs_quotes` of every field it writes, and that predicate
//! picks one of two implementations by field width: at and above
//! `SIMD_QUOTING_SCAN_BYTES`, which is 32, a SIMD block scan through `find3` or
//! `find4`; below it, a word-at-a-time loop covering eight bytes an iteration.
//! The threshold is a tuning decision, and until this suite existed nothing in
//! the repository could re-derive it — every quoting sentinel was a whole
//! document encode, where field widths are whatever the corpus happens to hold.
//!
//! So the `blocks` and `words` cases here call each arm directly, at every
//! width, including widths emission would never choose that arm at. Both are
//! correct everywhere, which is what makes the comparison possible. The
//! `dispatch` cases call the predicate as emission does and should track
//! whichever arm the threshold selects.
//!
//! Every case scans 1,000 fields that contain nothing needing quotes, so each
//! scan runs to the end of its field: this is the arms' worst case and their
//! only width-proportional one, since a field needing quotes exits early at
//! wherever the offending byte sits.
//!
//! Callgrind Ir for 1,000 fields, `Newline` (`find4`, four needles):
//!
//! | Width | `words` | `blocks` | `dispatch` | Blocks over words |
//! |------:|--------:|---------:|-----------:|------------------:|
//! | 8     |  47,063 |   59,047 |     38,057 |             1.25x |
//! | 16    |  76,063 |  107,047 |     61,057 |             1.41x |
//! | 24    | 105,063 |  155,047 |     89,057 |             1.48x |
//! | 32    | 134,063 |   48,355 |     48,366 |             0.36x |
//! | 64    | 250,063 |   62,355 |     62,366 |             0.25x |
//! | 128   | 482,063 |   90,355 |     90,366 |             0.19x |
//!
//! Callgrind Ir for 1,000 fields, `Byte(b';')` (`find3`, three needles):
//!
//! | Width | `words` | `blocks` | `dispatch` | Blocks over words |
//! |------:|--------:|---------:|-----------:|------------------:|
//! | 8     |  36,062 |   63,487 |     41,057 |             1.76x |
//! | 16    |  59,062 |  115,927 |     68,057 |             1.96x |
//! | 24    |  82,062 |  168,367 |     99,057 |             2.05x |
//! | 32    | 105,062 |   45,355 |     45,366 |             0.43x |
//! | 64    | 197,062 |   57,355 |     57,366 |             0.29x |
//! | 128   | 381,062 |   81,355 |     81,366 |             0.21x |
//!
//! The threshold is right, and the tables show it is not a tuning constant at
//! all. `blocks` does not approach `words` gradually and overtake it somewhere
//! in the middle: it is 1.3-2.1 times dearer at every width below 32 and 2.3-2.8
//! times cheaper the moment the width reaches 32, with a discontinuity between
//! the two rows — 155,047 to 48,355 under `Newline` — rather than a crossing.
//! That is `find` itself falling back to a byte-at-a-time scan when it has no
//! whole block to work on, so below 32 bytes the "SIMD" arm is not vectorized
//! and pays its block setup for nothing. 32 is therefore forced by the block
//! width, and the only threshold worth reconsidering would be a *higher* one —
//! which these numbers refute, since the block arm already wins by 2.3x at the
//! first width it can vectorize at.
//!
//! Above the threshold `blocks` grows at about 0.44 instructions per byte
//! against `words` at 2.9, so the gap widens with width: 2.8x at 32 and 5.3x at
//! 128 under `Newline`. `find3` is cheaper than `find4` at every width above
//! the threshold, by around 10%, which is the fourth needle costing what one
//! extra comparison per block should.
//!
//! `dispatch` tracks whichever arm the threshold picks, to within about 0.02%
//! above the threshold. Below it, `dispatch` is consistently *cheaper* than the
//! `words` case measuring the same code — 38,057 against 47,063 at width 8 —
//! because the dispatching predicate derives its needle set from a constant
//! dialect and folds the record-ending test away, while the benchmarking
//! wrapper hands the arm a runtime flag it cannot. The effect is a few
//! instructions per field, an order of magnitude below the crossover it might
//! otherwise be mistaken for.

#![expect(missing_docs, reason = "benchmark macros are private")]

use std::hint::black_box;

use coseva::benchmark::{needs_quotes, needs_quotes_blocks, needs_quotes_words};
use coseva::config::RecordEnding;
use gungraun::prelude::*;

const FIELDS: usize = 1_000;

/// The three-needle arm's dialect. `RecordEnding::Newline` and `CrLf` both scan
/// for `\n` *and* `\r`, so only a `Byte` ending reaches `find3`.
const BYTE_ENDING: RecordEnding = RecordEnding::Byte(b';');

const W8: usize = 8;
const W16: usize = 16;
const W24: usize = 24;
const W32: usize = 32;
const W64: usize = 64;
const W128: usize = 128;

/// `FIELDS` fields of `width` bytes, back to back, none of them needing quotes.
///
/// The filler cycles through letters and digits, so it holds no delimiter,
/// quote, newline or carriage return under any of the dialects measured here
/// and every scan therefore reads its whole field.
fn corpus(width: usize) -> (usize, Vec<u8>) {
    const FILLER: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = (0..width * FIELDS)
        .map(|index| FILLER[index % FILLER.len()])
        .collect();
    (width, bytes)
}

fn drop_it(input: (usize, Vec<u8>)) {
    drop(input);
}

/// Run `scan` over every field and assert none of them wanted quoting, so a
/// scan that stopped early cannot become a benchmark result.
fn sweep((width, bytes): (usize, Vec<u8>), scan: impl Fn(&[u8]) -> bool) -> (usize, Vec<u8>) {
    let quoted = bytes
        .chunks_exact(width)
        .filter(|field| scan(field))
        .count();
    assert_eq!(quoted, 0, "benchmark corpus must not need quoting");
    black_box((width, bytes))
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn newline_dispatch(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| needs_quotes(RecordEnding::Newline, field))
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn newline_blocks(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| {
        needs_quotes_blocks(RecordEnding::Newline, field)
    })
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn newline_words(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| {
        needs_quotes_words(RecordEnding::Newline, field)
    })
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn byte_dispatch(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| needs_quotes(BYTE_ENDING, field))
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn byte_blocks(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| needs_quotes_blocks(BYTE_ENDING, field))
}

#[library_benchmark]
#[bench::w8(args = (W8), setup = corpus, teardown = drop_it)]
#[bench::w16(args = (W16), setup = corpus, teardown = drop_it)]
#[bench::w24(args = (W24), setup = corpus, teardown = drop_it)]
#[bench::w32(args = (W32), setup = corpus, teardown = drop_it)]
#[bench::w64(args = (W64), setup = corpus, teardown = drop_it)]
#[bench::w128(args = (W128), setup = corpus, teardown = drop_it)]
fn byte_words(input: (usize, Vec<u8>)) -> (usize, Vec<u8>) {
    sweep(input, |field| needs_quotes_words(BYTE_ENDING, field))
}

library_benchmark_group!(
    name = quoting;
    benchmarks =
        newline_dispatch,
        newline_blocks,
        newline_words,
        byte_dispatch,
        byte_blocks,
        byte_words
);

main!(library_benchmark_groups = quoting);
