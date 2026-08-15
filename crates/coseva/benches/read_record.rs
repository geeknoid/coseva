//! Reading records, through each front end, next to `csv-core`.
//!
//! Each corpus is a `static` built at compile time from copies of one unquoted
//! six-field record, so every benchmark parses exactly the same bytes into
//! exactly the same fields and returns the same checksum. Running 1, 10, 100
//! and 1000 records separates the two things a single size conflates: the
//! fixed cost a front end pays once, and the marginal cost it pays per record.
//! Only the second is a parsing speed.
//!
//! Differencing the sizes gives the marginal cost directly, and it is flat
//! across every decade, which is what says the corpora are large enough to be
//! measuring steady state rather than warm-up:
//!
//! | Case       |    1 |    10 |   100 |   1000 | Per record |
//! |------------|------|-------|-------|--------|------------|
//! | `slice`    |  775 |  4934 | 45821 | 455265 |        455 |
//! | `push`     | 1347 |  5884 | 50551 | 497795 |        497 |
//! | `csv_core` | 1250 |  9575 | 92825 | 925325 |        925 |
//!
//! # Why `IoParser` is not here
//!
//! `csv-core` is a raw
//! state machine that never reads anything, and `slice` and `push` are handed
//! bytes that are already in memory. `IoParser` reads from a source and owns a
//! window, so most of what would separate its column from the others is
//! buffering
//! rather than parsing — and Callgrind measures buffering particularly badly,
//! charging `rep movsb` an instruction per byte where a vectorized copy costs
//! one per thirty-two. Nudging a buffer size across one of
//! glibc's internal thresholds is enough to move such a row by 5% with the
//! machine copying exactly as many bytes as before.
//!
//! `IoParser` appears in the `byte_record` and `text_record` tables
//! instead, next to the `csv` crate, which also buffers, also reads, and also
//! returns an owned record. That is a comparison between two things of the
//! same shape, which this one is not.
//!
//! # What is and is not measured
//!
//! Constructing a parser allocates its buffers and resolves its dialect, and
//! dropping one frees those buffers. Neither is per-record work, so neither is
//! measured: `setup` builds the parser before Callgrind starts counting, and
//! the benchmark hands it back so `teardown` drops it after counting stops.
//! What remains inside the measured region is reading the records and walking
//! their fields.
//!
//! # Reading the `push` number
//!
//! `PushParser` and `csv-core` are the same shape of design, and coseva hands
//! back slices where `csv-core` copies, so `push` should win. It does at every
//! size past a single record, and by 46% at a thousand.
//!
//! It loses at one record, where the fixed costs of setting a parser
//! running outweigh a single record's worth of copying. The dominant fixed
//! costs are not parsing, and the one-record ratio of 1.08x depends on
//! avoiding both. Opening a chunk in absorbing mode with nothing to absorb
//! spends a round
//! parsing an empty window before handing the chunk to the borrowed path; a
//! record whose terminator lands on the chunk's last byte, if rewound, copied
//! into the window and parsed a second time from the copy, is half the total
//! work at one record and the only reason the parser
//! allocates at all. Recognizing such a record as whole avoids the second
//! parse, the copy and the allocation together.
//!
//! Against `slice` the steady-state difference is framing rather than parsing,
//! and this is worth stating precisely because it is easy to assume otherwise.
//! The parse kernel is the same code doing the same work: 371,479 instructions
//! against 371,026, a tenth of a percent apart. Every instruction separating
//! the two front ends is spent around that kernel, not inside it, which means
//! there is nothing to gain here by making the parser faster.
//!
//! What is left is 42 instructions per record of accepting partial input:
//! deciding whether a record that reaches the end of a chunk is whole, holding
//! the loan on the caller's bytes, and the `Advance` plumbing that reports
//! truncation back to the caller. `slice` pays none of it because it borrows
//! input it knows is complete. Holding the gap at 42 rather than 113
//! instructions per record rests on counting a record's newline while
//! parsing it rather than rescanning the bytes being dropped, forcing the
//! window advance inline so it stops paying a call frame, fusing the borrowed
//! path so the loan is tested once instead of twice, and cutting the rewind
//! snapshot down to the four fields that cannot be recovered from the cursor.
//!
//! The `chunk` gap is not a bulk `memcpy`, though a naive reading of the
//! profile suggests 51 instructions per record for one. Callgrind
//! charges `rep movsb` a whole instruction for every byte, so a single 51 KB
//! copy reads as 51,234 instructions when the true cost is a few microseconds
//! of bandwidth. Removing the copy altogether, which is what `chunk` does,
//! buys 9 instructions per record rather than 51. Instruction counts cannot
//! see memory traffic, and a 51 KB corpus that fits in L2 cache cannot show it
//! either, so the case for `chunk` rests on a measurement this file does not
//! take.
//!
//! # Fairness
//!
//! All three cases use the CSV dialect through coseva's compile-time [`Csv`]
//! format, take their input from the same `static`, iterate fields in index
//! order, and return the same sum of field lengths. The checksum is asserted
//! rather than assumed, so a case cannot quietly drift into parsing something
//! else and still look comparable.
//!
//! The `csv-core` case is the closest external equivalent, not an identical
//! one, and the difference cuts both ways. `csv-core` copies each field into a
//! caller-supplied output buffer, where coseva's borrowed records hand back
//! slices pointing into the input; that copy is work coseva does not do. In
//! exchange `csv-core` is a raw state machine with no header handling, no
//! field-count validation, and no record bookkeeping. Read the gap as "these
//! two are in the same ballpark", not as a like-for-like ratio.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{Chunk, PushParser, SliceParser};
use gungraun::prelude::*;

/// One unquoted six-field record, terminated by a newline.
const ROW: &[u8] = b"Boston,Massachusetts,4500000,42.3601,-71.0589,true\n";

/// The width of [`ROW`], which every corpus is an exact multiple of.
const ROW_LEN: usize = ROW.len();

/// The sum of the six field lengths in [`ROW`], excluding its delimiters.
const FIELD_BYTES: u64 = 6 + 13 + 7 + 7 + 8 + 4;

/// Build a corpus of `N / ROW_LEN` copies of [`ROW`] at compile time.
///
/// Generating the corpus in a `const fn` keeps every fixture a `static`, so no
/// case pays for building or allocating its input, in the measured region or
/// out of it.
const fn corpus<const N: usize>() -> [u8; N] {
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

static ROWS_1: &[u8] = &BUF_1;
static ROWS_10: &[u8] = &BUF_10;
static ROWS_100: &[u8] = &BUF_100;
static ROWS_1000: &[u8] = &BUF_1000;

/// The read buffer the options carry.
///
/// No case here reads from a source any more, so nothing refills and this only
/// keeps the parse options identical to the ones the owned-record benchmarks
/// use. It is the `csv` crate's default, set explicitly there so that
/// comparison cannot silently become one of default buffer sizes.
const BUFFER: usize = 8 * 1024;

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(BUFFER)
}

/// Assert the case walked every field of every record in `input`.
fn check(total: u64, input: &[u8]) -> u64 {
    let expected = (input.len() / ROW_LEN) as u64 * FIELD_BYTES;
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    total
}

// ── setup: everything that is not per-record work ────────────────────────────

fn slice_parser(input: &'static [u8]) -> (SliceParser<'static, Csv>, &'static [u8]) {
    let parser = SliceParser::<Csv>::new(input, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, input)
}

fn push_parser(input: &'static [u8]) -> (PushParser<Csv>, &'static [u8]) {
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, input)
}

/// A `csv-core` reader plus the output buffers it writes fields into.
///
/// coseva allocates its buffers when the parser is built, so `csv-core`'s
/// equivalent storage is allocated here too rather than inside the measured
/// region.
// The generated benchmark module below takes the `csv_core` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn core_reader(input: &'static [u8]) -> (::csv_core::Reader, Vec<u8>, Vec<usize>, &'static [u8]) {
    (
        ::csv_core::ReaderBuilder::new().build(),
        vec![0; ROW_LEN],
        vec![0; 16],
        input,
    )
}

// ── teardown: drop outside the measured region ───────────────────────────────

fn drop_it<T>(value: T) {
    drop(value);
}

// ── the measured bodies ──────────────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_parser, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_parser, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_parser, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_parser, teardown = drop_it)]
fn slice(state: (SliceParser<'static, Csv>, &'static [u8])) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        for index in 0..record.len() {
            total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
        }
    }
    (black_box(check(total, input)), parser)
}

// The push front end, handed the whole record in one chunk. `finish` is inside the
// measured region because it is what tells the parser the unterminated tail is
// complete; without it the final record of a stream never arrives, so it is
// per-record work rather than teardown.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = push_parser, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = push_parser, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = push_parser, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = push_parser, teardown = drop_it)]
fn push(state: (PushParser<Csv>, &'static [u8])) -> (u64, PushParser<Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..]);
        total = total.wrapping_add(drain(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk));
    let _ = chunk.done();
    (black_box(check(total, input)), parser)
}

fn drain(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = chunk
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        for index in 0..record.len() {
            total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
        }
    }
    total
}

// `csv-core` reading the same record into its caller-supplied buffers. The loop
// mirrors the coseva cases: read one record, then walk its fields in index
// order summing their lengths. `csv-core` reports field ends rather than
// fields, so the lengths come from successive differences.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = core_reader, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = core_reader, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = core_reader, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = core_reader, teardown = drop_it)]
fn csv_core(
    state: (::csv_core::Reader, Vec<u8>, Vec<usize>, &'static [u8]),
) -> (u64, (::csv_core::Reader, Vec<u8>, Vec<usize>)) {
    let (mut reader, mut output, mut ends, corpus) = state;
    let mut total = 0_u64;
    let mut input: &[u8] = corpus;
    loop {
        let (result, read, _written, fields) = reader.read_record(input, &mut output, &mut ends);
        input = &input[read..];
        match result {
            ::csv_core::ReadRecordResult::Record => {
                let mut start = 0;
                for &end in &ends[..fields] {
                    total = total.wrapping_add((end - start) as u64);
                    start = end;
                }
            }
            ::csv_core::ReadRecordResult::End => break,
            ::csv_core::ReadRecordResult::InputEmpty => {}
            other => panic!("benchmark input failed: {other:?}"),
        }
    }
    (black_box(check(total, corpus)), (reader, output, ends))
}

library_benchmark_group!(
    name = read_record;
    benchmarks = slice, push, csv_core
);

main!(library_benchmark_groups = read_record);
