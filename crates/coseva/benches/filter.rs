//! Filtering by a column predicate, across selectivities.
//!
//! [`next_matching_line`] exists to skip work: it tests one column and only
//! materializes a record when that column matches. How much that is worth
//! depends entirely on how often the predicate is true, which is why
//! selectivity rather than record count is this suite's axis.
//!
//! # The comparison
//!
//! Every case scans the same 1000 records and counts the matches. The slice
//! cases cover predicates on the first and last columns, the io cases run the
//! separate pushdown the I/O front end implements, and the push cases feed
//! 32-byte chunks so every 51-byte record crosses a chunk boundary, with a
//! 4 KiB variant beside them so chunk-copy cost and filter cost are separable.
//! `manual` reads every record through [`next_line`] and tests column 0 itself,
//! which is what a caller writes without the filter; `csv` does the same
//! through the `csv` crate, which has no filtering path of its own.
//!
//! `manual` is the row that matters. `filtered` beating `csv` would say little
//! — the two crates differ for reasons this suite is not about — but `filtered`
//! against `manual` is the same crate over the same bytes with only the filter
//! changed, so their difference is what the filter actually buys.
//!
//! # The two predicates
//!
//! `equals` compares the whole field. `contains` runs the SIMD literal search
//! in `search.rs`, which is a different code path and the one with the more
//! interesting failure mode: a needle that is absent must be proven absent, so
//! the search cannot stop early on a miss.
//!
//! # The corpus
//!
//! 1000 records of a fixed 51 bytes. Matching records contain `Boston` in
//! column 0 and `true` in column 5; misses contain the same-width `Austin` and
//! `nope`. All three corpora are therefore byte-identical in size and every
//! case scans exactly the same number of bytes. Each pair also serves both the
//! equality and contains predicates.
//!
//! Selectivity is `all` (every record matches), `hundredth` (10 matches) and
//! `thousandth` (1 match). Each case asserts its match count, so a filter that
//! stopped matching would fail rather than post a better number.
//!
//! # Results
//!
//! Callgrind instruction counts for one scan of 1000 records.
//!
//! `equals`:
//!
//! | Case       | all       | hundredth | thousandth |
//! |------------|-----------|-----------|------------|
//! | `filtered` |   496,099 |    74,044 |     66,749 |
//! | `manual`   |   448,278 |   444,193 |    444,164 |
//! | `csv`      | 1,031,494 | 1,027,534 |  1,027,498 |
//!
//! `contains`, which runs the SIMD literal search:
//!
//! | Case       | all     | hundredth | thousandth |
//! |------------|---------|-----------|------------|
//! | `filtered` | 538,984 |    75,880 |     68,045 |
//! | `manual`   | 487,153 |   520,566 |    520,891 |
//!
//! # What the numbers say
//!
//! The filter is worth a great deal when it rejects, and still costs something
//! when it does not. At one match in a thousand it runs 6.7× cheaper than
//! testing every record by hand — 66,749 against 444,164 — and 15× cheaper
//! than the `csv` crate. At every record matching it is 11% *dearer* than
//! doing it by hand, 496,099 against 448,278. `contains` behaves the same way,
//! 7.7× cheaper at one in a thousand and 11% dearer at all.
//!
//! Those two rows fit a line. Rejecting a record costs about 66 instructions
//! and accepting one about 430, against a flat 444 to parse and test a record
//! by hand. So the filter pays for itself up to roughly 89% selectivity and
//! loses only above it. `next_matching_line` says so, on all three
//! parsers, because a threshold that high is one most callers reaching for a
//! filter are on the winning side of and none of them can otherwise
//! know about.
//!
//! # The record is not parsed twice
//!
//! It is tempting to read the `all` column as the cost of a second pass.
//! Profiling says otherwise: the
//! accepted record is parsed exactly once, because `advance_with_filter`
//! publishes `cursor_end` and `materialize_full` reuses the extent rather than
//! re-parsing. `fill_record_spans` accounts for 7% of the profile, not 50%.
//!
//! Three things keep the filter path off the costs a naive implementation
//! would carry. `fill_record_spans` is generic over `F`, so filtering a
//! `SliceParser<Csv>` runs the monomorphized parser rather than the
//! dynamically-dispatched one, worth 8%. `Predicate::is_skippable`, asked
//! once per record whether the literal can be searched for in raw input,
//! answers from a byte set precomputed at construction rather than rescanning
//! the literal, five bitset tests instead, worth a further
//! 18%. And the loop resolves the candidate field's span directly rather than
//! building a whole `Record` to read one field from, four of that type's six
//! fields having no bearing on
//! reading a field; that is worth a further 2.9% on
//! `equals` and 2.7% on `contains`, with every `manual` row unmoved.
//!
//! What remains is about 48 instructions per accepted record for the filter's
//! own bookkeeping. No single piece of that has been identified, and at 11% of
//! a hand-written loop it is small enough that the crossover is documented
//! rather than chased.
//!
//! `contains`'s `manual` row inverts, costing more when it misses — 520,891
//! against 487,153 — because an absent needle has to be proven absent across
//! the whole field, while a present one stops at the match. `equals` shows
//! nothing of the sort, being flat to within 1%, since a comparison against
//! `Austin` fails on its first byte.
//!
//! Against `csv`, which is flat because it has no filtering path to vary,
//! everything here wins: the hand-written coseva loop by 57% and the filter by
//! anything from 50% to 15×, depending on how much it gets to skip.
//!
//! # The three front ends do not filter alike
//!
//! The same predicate over the same bytes, by front end, at the extremes of
//! selectivity:
//!
//! | Front end          | all     | thousandth | Ratio |
//! |--------------------|---------|------------|-------|
//! | `slice`            | 496,099 |     66,749 |  7.4× |
//! | `io`               | 616,046 |    390,268 |  1.6× |
//! | `push` (4 KiB)     | 553,948 |    540,835 |  1.02×|
//! | `push` (32 B)      | 2.67 M  |     2.67 M |  1.00×|
//!
//! The ratio column is what the filter buys on that front end, and the three
//! rows are three different answers. The slice path skips nearly all the work
//! of a rejected record. The io path skips some. The push path skips none: at
//! 4 KiB chunks, where whole records sit inside one chunk and per-chunk copying
//! no longer dominates, one match in a thousand costs within 2% of every record
//! matching. That is the measurement saying the push front end has no pushdown,
//! rather than having one that performs badly.
//!
//! The 32-byte push rows cannot show this at all, which is why the 4 KiB ones
//! exist: at that size the copy is 80% of the number and swamps the rest.
//!
//! On io, `equals` at one in a thousand costs 390,268 while `contains` costs
//! 258,001 — the cheaper-looking predicate is the dearer one, which is the
//! reverse of the slice path, where the two are within 2%.
//!
//! # Long records, where skipping is most of the cost
//!
//! `long_*` replaces the 51-byte record with a 2 KiB one carrying a single wide
//! filler column, so a rejected record is a scan across many vector blocks
//! rather than a few bytes:
//!
//! | Case                   | all     | sparse  |
//! |------------------------|---------|---------|
//! | `long_equals`          | 542,555 | 360,561 |
//! | `long_contains`        | 575,051 | 347,691 |
//! | `long_late_equals`     | 711,538 | 367,296 |
//! | `long_late_contains`   | 738,226 | 370,041 |
//!
//! `sparse` matches one record in 128 and still costs two thirds of what
//! accepting all of them does, though it splits no fields and materializes one
//! record. At this record length the skip scan, not the parse it avoids, is
//! what the number is made of — which the 51-byte cases cannot show, their
//! `thousandth` column falling to an eighth.
//!
//! The `long_late_*` pair puts the predicate on the last column instead of the
//! first, which moves the candidate hit to the end of its record and so leaves
//! the span handed to the backward terminator search ending about 2 KiB past
//! the previous record ending. That is the only case in the suite where
//! `rfind1` widens its 128-byte initial window rather than finding what it
//! wants in the first pass. Raising that window to 4 KiB moves
//! `long_late_equals::sparse` by 4,514 instructions and leaves
//! `long_equals::sparse` unmoved to within 7, which is how the two are known to
//! be measuring different things: about 35 instructions per skipped record is
//! what the widening currently costs.
//!
//! # What this does not measure
//!
//! The suite deliberately fixes record width within each corpus and uses two
//! push chunk sizes. It does not sweep field width or column count; those are
//! separate scaling axes.
//!
//! Numbers in this file are comparable only to each other; `fixture.rs`
//! records the measurement showing why.
//!
//! [`next_matching_line`]: coseva::SliceParser::next_matching_line
//! [`next_line`]: coseva::SliceParser::next_line

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::Predicate;
use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, IoParser, PushParser, SliceParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this suite builds its own two-valued corpus and counts matches instead of bytes"
)]
mod fixture;

use fixture::{BUFFER, FIELDS, drop_it};

/// The bytes between the first and last fields.
const MIDDLE: &[u8] = b",Massachusetts,4500000,42.3601,-71.0589,";

/// Column 0 of a record the predicates match.
const EARLY_HIT: &[u8] = b"Boston";

/// Column 0 of a record they do not, the same length as [`EARLY_HIT`].
const EARLY_MISS: &[u8] = b"Austin";

/// Column 5 of a record the predicates match.
const LATE_HIT: &[u8] = b"true";

/// Column 5 of a record they do not, the same length as [`LATE_HIT`].
const LATE_MISS: &[u8] = b"nope";

/// The width of one record, fixed because both hit/miss pairs agree.
const ROW_LEN: usize = EARLY_HIT.len() + MIDDLE.len() + LATE_HIT.len() + 1;

/// The number of records every case scans.
const ROWS: usize = 1000;

/// The early-column needle, present in [`EARLY_HIT`] and absent from
/// [`EARLY_MISS`].
const EARLY_NEEDLE: &[u8] = b"osto";

/// The late-column needle, present in [`LATE_HIT`] and absent from
/// [`LATE_MISS`].
const LATE_NEEDLE: &[u8] = b"tru";

/// Smaller than one record, forcing every push record across a chunk boundary.
const PUSH_CHUNK: usize = 32;

/// Larger than one record by enough that most records sit whole inside a chunk.
///
/// At [`PUSH_CHUNK`] the per-chunk copy dominates and hides whatever the filter
/// does, so the two sizes are measured separately rather than averaged: this
/// one is where a pushdown on the push front end would become visible.
const PUSH_CHUNK_LARGE: usize = 4096;

/// The filler column that makes a long record long.
///
/// It carries neither a separator nor a terminator, so one record spans many
/// vector blocks and a skipped record forces the scan to cross all of them.
const LONG_FILL: usize = 2000;

/// The width of one long record: predicate column, filler column, flag column.
const LONG_ROW_LEN: usize = EARLY_HIT.len() + 1 + LONG_FILL + 1 + LATE_HIT.len() + 1;

/// The number of long records, chosen to keep the corpus near the 256 KiB the
/// shared document budget uses.
const LONG_ROWS: usize = 128;

/// A corpus of `N / ROW_LEN` records in which every `STRIDE`-th one matches.
///
/// The length is a const parameter rather than computed from [`ROWS`] so that
/// the buffer is not a concrete large array at the point it is declared, which
/// is the same shape `fixture.rs` uses.
const fn corpus<const N: usize, const STRIDE: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut record = 0;
    while record * ROW_LEN < N {
        let matches = record % STRIDE == 0;
        let city = if matches { EARLY_HIT } else { EARLY_MISS };
        let flag = if matches { LATE_HIT } else { LATE_MISS };
        let base = record * ROW_LEN;
        let mut index = 0;
        while index < EARLY_HIT.len() {
            out[base + index] = city[index];
            index += 1;
        }
        let mut offset = 0;
        while offset < MIDDLE.len() {
            out[base + EARLY_HIT.len() + offset] = MIDDLE[offset];
            offset += 1;
        }
        let flag_base = base + EARLY_HIT.len() + MIDDLE.len();
        offset = 0;
        while offset < LATE_HIT.len() {
            out[flag_base + offset] = flag[offset];
            offset += 1;
        }
        out[flag_base + LATE_HIT.len()] = b'\n';
        record += 1;
    }
    out
}

static ALL: [u8; ROW_LEN * ROWS] = corpus::<{ ROW_LEN * ROWS }, 1>();
static HUNDREDTH: [u8; ROW_LEN * ROWS] = corpus::<{ ROW_LEN * ROWS }, 100>();
static THOUSANDTH: [u8; ROW_LEN * ROWS] = corpus::<{ ROW_LEN * ROWS }, 1000>();

/// A corpus of long records in which every `STRIDE`-th one matches.
///
/// The filler column is left as the `x` the array is initialized with, so the
/// only structural bytes in a record are its two separators and its ending.
const fn long_corpus<const N: usize, const STRIDE: usize>() -> [u8; N] {
    let mut out = [b'x'; N];
    let mut record = 0;
    while record * LONG_ROW_LEN < N {
        let matches = record % STRIDE == 0;
        let city = if matches { EARLY_HIT } else { EARLY_MISS };
        let flag = if matches { LATE_HIT } else { LATE_MISS };
        let base = record * LONG_ROW_LEN;
        let mut index = 0;
        while index < EARLY_HIT.len() {
            out[base + index] = city[index];
            index += 1;
        }
        out[base + EARLY_HIT.len()] = b',';
        let flag_base = base + EARLY_HIT.len() + 1 + LONG_FILL;
        out[flag_base] = b',';
        let mut offset = 0;
        while offset < LATE_HIT.len() {
            out[flag_base + 1 + offset] = flag[offset];
            offset += 1;
        }
        out[flag_base + 1 + LATE_HIT.len()] = b'\n';
        record += 1;
    }
    out
}

static LONG_ALL: [u8; LONG_ROW_LEN * LONG_ROWS] = long_corpus::<{ LONG_ROW_LEN * LONG_ROWS }, 1>();
static LONG_SPARSE: [u8; LONG_ROW_LEN * LONG_ROWS] =
    long_corpus::<{ LONG_ROW_LEN * LONG_ROWS }, LONG_ROWS>();

/// The corpus and the number of its records that match, so every case can
/// assert it counted them all.
type Input = (&'static [u8], u64);

static ALL_IN: Input = (&ALL, 1000);
static HUNDREDTH_IN: Input = (&HUNDREDTH, 10);
static THOUSANDTH_IN: Input = (&THOUSANDTH, 1);

static LONG_ALL_IN: Input = (&LONG_ALL, LONG_ROWS as u64);
static LONG_SPARSE_IN: Input = (&LONG_SPARSE, 1);

fn check(found: u64, expected: u64) -> u64 {
    assert_eq!(found, expected, "benchmark matched the wrong records");
    found
}

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(BUFFER)
}

type SliceState = (SliceParser<'static, Csv>, Predicate, u64);

fn equals_state(input: Input) -> SliceState {
    state(input, Predicate::equals(0, EARLY_HIT))
}

fn late_equals_state(input: Input) -> SliceState {
    state(input, Predicate::equals(5, LATE_HIT))
}

/// The same predicate keyed by header name instead of index.
///
/// Headers are *provided* rather than read from the corpus so the bytes
/// scanned stay identical to the indexed cases and the two are directly
/// comparable: the difference is the column resolution and nothing else.
fn named_state(input: Input) -> SliceState {
    let (bytes, expected) = input;
    let headers = ByteRecord::from_iter([&b"city"[..], b"state", b"pop", b"lat", b"lon", b"flag"]);
    let parser = SliceParser::<Csv>::new(
        bytes,
        ParseOptions::new()
            .headers(Headers::Provided(headers))
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, Predicate::equals("city", EARLY_HIT), expected)
}

fn contains_state(input: Input) -> SliceState {
    state(input, Predicate::contains(0, EARLY_NEEDLE))
}

fn late_contains_state(input: Input) -> SliceState {
    state(input, Predicate::contains(5, LATE_NEEDLE))
}

fn state(input: Input, predicate: Predicate) -> SliceState {
    let (bytes, expected) = input;
    let parser = SliceParser::<Csv>::new(bytes, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, predicate, expected)
}

// ── filtered ─────────────────────────────────────────────────────────────────

fn run_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    let (mut parser, predicate, expected) = state;
    let mut found = 0_u64;
    while let Some(mut line) = parser
        .next_matching_line(&predicate)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        found = found.wrapping_add(record.len() as u64 / FIELDS as u64);
    }
    (black_box(check(found, expected)), parser, predicate)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = equals_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = equals_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = equals_state, teardown = drop_it)]
fn equals_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = contains_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = contains_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = contains_state, teardown = drop_it)]
fn contains_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = late_equals_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = late_equals_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = late_equals_state, teardown = drop_it)]
fn late_equals_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = late_contains_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = late_contains_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = late_contains_state, teardown = drop_it)]
fn late_contains_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_filtered(state)
}

// ── push: every record crosses a chunk boundary ──────────────────────────────

type PushState = (PushParser<Csv>, Predicate, &'static [u8], u64);

fn push_state(input: Input, predicate: Predicate) -> PushState {
    let (bytes, expected) = input;
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, predicate, bytes, expected)
}

fn push_equals_state(input: Input) -> PushState {
    push_state(input, Predicate::equals(0, EARLY_HIT))
}

fn push_late_equals_state(input: Input) -> PushState {
    push_state(input, Predicate::equals(5, LATE_HIT))
}

fn push_contains_state(input: Input) -> PushState {
    push_state(input, Predicate::contains(0, EARLY_NEEDLE))
}

fn push_late_contains_state(input: Input) -> PushState {
    push_state(input, Predicate::contains(5, LATE_NEEDLE))
}

fn drain_filtered(chunk: &mut Chunk<'_, '_, Csv>, predicate: &Predicate) -> u64 {
    let mut found = 0_u64;
    while let Some(mut line) = chunk
        .next_matching_line(predicate)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        found = found.wrapping_add(record.len() as u64 / FIELDS as u64);
    }
    found
}

fn run_push_filtered(state: PushState, chunk_bytes: usize) -> (u64, PushParser<Csv>, Predicate) {
    let (mut parser, predicate, input, expected) = state;
    let mut found = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = (fed + chunk_bytes).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        found = found.wrapping_add(drain_filtered(&mut chunk, &predicate));
        let done = chunk.done();
        assert!(done > 0, "benchmark push parser made no progress");
        fed += done;
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    found = found.wrapping_add(drain_filtered(&mut chunk, &predicate));
    let _ = chunk.done();
    (black_box(check(found, expected)), parser, predicate)
}

macro_rules! push_benchmark {
    ($name:ident, $setup:ident, $chunk:expr) => {
        #[library_benchmark]
        #[bench::all(args = (ALL_IN), setup = $setup, teardown = drop_it)]
        #[bench::hundredth(args = (HUNDREDTH_IN), setup = $setup, teardown = drop_it)]
        #[bench::thousandth(args = (THOUSANDTH_IN), setup = $setup, teardown = drop_it)]
        fn $name(state: PushState) -> (u64, PushParser<Csv>, Predicate) {
            run_push_filtered(state, $chunk)
        }
    };
}

push_benchmark!(push_equals_filtered, push_equals_state, PUSH_CHUNK);
push_benchmark!(
    push_late_equals_filtered,
    push_late_equals_state,
    PUSH_CHUNK
);
push_benchmark!(push_contains_filtered, push_contains_state, PUSH_CHUNK);
push_benchmark!(
    push_late_contains_filtered,
    push_late_contains_state,
    PUSH_CHUNK
);

// The same work at a chunk size spanning about eighty records, so the per-chunk
// copy no longer dominates and the filter's own cost is separable.
push_benchmark!(
    push_equals_filtered_large,
    push_equals_state,
    PUSH_CHUNK_LARGE
);
push_benchmark!(
    push_contains_filtered_large,
    push_contains_state,
    PUSH_CHUNK_LARGE
);

// ── io: the front end with its own pushdown implementation ───────────────────

type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, Predicate, u64);

fn io_state(input: Input, predicate: Predicate) -> IoState {
    let (bytes, expected) = input;
    let parser = IoParser::<_, Csv>::new(Cursor::new(bytes), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, predicate, expected)
}

fn io_equals_state(input: Input) -> IoState {
    io_state(input, Predicate::equals(0, EARLY_HIT))
}

fn io_contains_state(input: Input) -> IoState {
    io_state(input, Predicate::contains(0, EARLY_NEEDLE))
}

fn run_io_filtered(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, Predicate) {
    let (mut parser, predicate, expected) = state;
    let mut found = 0_u64;
    while let Some(mut line) = parser
        .next_matching_line(&predicate)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        found = found.wrapping_add(record.len() as u64 / FIELDS as u64);
    }
    (black_box(check(found, expected)), parser, predicate)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = io_equals_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = io_equals_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = io_equals_state, teardown = drop_it)]
fn io_equals_filtered(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, Predicate) {
    run_io_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = io_contains_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = io_contains_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = io_contains_state, teardown = drop_it)]
fn io_contains_filtered(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, Predicate) {
    run_io_filtered(state)
}

// ── long records, where skipping is most of the work ─────────────────────────

/// The long corpus counts matched records rather than fields, because a long
/// record has three columns and the short one has six.
fn run_long_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    let (mut parser, predicate, expected) = state;
    let mut found = 0_u64;
    while let Some(mut line) = parser
        .next_matching_line(&predicate)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        found = found.wrapping_add(u64::from(record.len() == 3));
    }
    (black_box(check(found, expected)), parser, predicate)
}

/// The long corpus's flag column, roughly 2 KiB into the record.
///
/// A predicate here puts the candidate hit at the *end* of its record, so the
/// span the skip path hands to the backward terminator search ends far past the
/// previous record ending. That is what makes `rfind1` widen its 128-byte
/// initial window — twice, at this record length — which a hit in column 0
/// never does, the ending it wants being a few bytes back.
const LONG_LATE_COLUMN: usize = 2;

fn long_late_equals_state(input: Input) -> SliceState {
    state(input, Predicate::equals(LONG_LATE_COLUMN, LATE_HIT))
}

fn long_late_contains_state(input: Input) -> SliceState {
    state(input, Predicate::contains(LONG_LATE_COLUMN, LATE_NEEDLE))
}

// A skipped 2 KiB record is a scan across every block it occupies, so these are
// the cases in which the cost of the skip path, rather than of parsing what it
// accepts, is what the number is made of.
#[library_benchmark]
#[bench::all(args = (LONG_ALL_IN), setup = equals_state, teardown = drop_it)]
#[bench::sparse(args = (LONG_SPARSE_IN), setup = equals_state, teardown = drop_it)]
fn long_equals_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_long_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (LONG_ALL_IN), setup = contains_state, teardown = drop_it)]
#[bench::sparse(args = (LONG_SPARSE_IN), setup = contains_state, teardown = drop_it)]
fn long_contains_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_long_filtered(state)
}

// The two cases that drive the backward search's widening path.
#[library_benchmark]
#[bench::all(args = (LONG_ALL_IN), setup = long_late_equals_state, teardown = drop_it)]
#[bench::sparse(args = (LONG_SPARSE_IN), setup = long_late_equals_state, teardown = drop_it)]
fn long_late_equals_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_long_filtered(state)
}

#[library_benchmark]
#[bench::all(args = (LONG_ALL_IN), setup = long_late_contains_state, teardown = drop_it)]
#[bench::sparse(args = (LONG_SPARSE_IN), setup = long_late_contains_state, teardown = drop_it)]
fn long_late_contains_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_long_filtered(state)
}

// The cost of a name-keyed predicate, which resolves its column once per run
// and then compares the cached name per record it accepts.
#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = named_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = named_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = named_state, teardown = drop_it)]
fn named_filtered(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_filtered(state)
}

// ── manual: what the caller writes without the filter ────────────────────────

fn run_manual(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    let (mut parser, predicate, expected) = state;
    let mut found = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        if predicate.matches_field(record.get(0)) {
            found = found.wrapping_add(record.len() as u64 / FIELDS as u64);
        }
    }
    (black_box(check(found, expected)), parser, predicate)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = equals_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = equals_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = equals_state, teardown = drop_it)]
fn equals_manual(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_manual(state)
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = contains_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = contains_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = contains_state, teardown = drop_it)]
fn contains_manual(state: SliceState) -> (u64, SliceParser<'static, Csv>, Predicate) {
    run_manual(state)
}

// ── the csv crate, which has no filtering path ───────────────────────────────

type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    Predicate,
    u64,
);

fn csv_state(input: Input) -> CsvState {
    let (bytes, expected) = input;
    let reader = ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(bytes));
    (
        reader,
        ::csv::ByteRecord::with_capacity(ROW_LEN, FIELDS),
        Predicate::equals(0, EARLY_HIT),
        expected,
    )
}

#[library_benchmark]
#[bench::all(args = (ALL_IN), setup = csv_state, teardown = drop_it)]
#[bench::hundredth(args = (HUNDREDTH_IN), setup = csv_state, teardown = drop_it)]
#[bench::thousandth(args = (THOUSANDTH_IN), setup = csv_state, teardown = drop_it)]
fn csv_manual(
    state: CsvState,
) -> (
    u64,
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    Predicate,
) {
    let (mut reader, mut record, predicate, expected) = state;
    let mut found = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        if predicate.matches_field(record.get(0)) {
            found = found.wrapping_add(record.len() as u64 / FIELDS as u64);
        }
    }
    (black_box(check(found, expected)), reader, record, predicate)
}

library_benchmark_group!(
    name = equals;
    benchmarks =
        equals_filtered,
        late_equals_filtered,
        push_equals_filtered,
        push_late_equals_filtered,
        push_equals_filtered_large,
        io_equals_filtered,
        long_equals_filtered,
        long_late_equals_filtered,
        named_filtered,
        equals_manual,
        csv_manual
);

library_benchmark_group!(
    name = contains;
    benchmarks =
        contains_filtered,
        late_contains_filtered,
        push_contains_filtered,
        push_late_contains_filtered,
        push_contains_filtered_large,
        io_contains_filtered,
        long_contains_filtered,
        long_late_contains_filtered,
        contains_manual
);

main!(library_benchmark_groups = equals, contains);
