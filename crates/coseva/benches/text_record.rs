//! Reading records into a reusable owned [`TextRecord`], next to the `csv` crate.
//!
//! The UTF-8 counterpart to `byte_record`. Every case does the same work as
//! that table plus validation, and hands back a record whose fields are `&str`
//! rather than `&[u8]`. Differencing the two tables is the cost of validation
//! for each front end.
//!
//! | Case    |    1 |    10 |    100 |    1000 | Per record |
//! |---------|------|-------|--------|---------|------------|
//! | `slice` | 1170 |  8711 |  83394 |  830785 |        830 |
//! | `push`  | 1882 | 12952 | 102319 |  996820 |        994 |
//! | `io`    | 1683 | 12915 | 103465 | 1015024 |       1013 |
//! | `csv`   | 2774 | 12973 | 114745 | 1133488 |       1132 |
//!
//! # What validation costs
//!
//! Differencing against `byte_record` isolates it. Validation costs `csv` 57
//! instructions per record and costs coseva between 37 and 39, so an owned
//! text record costs its parse plus a validation pass and very little else.
//!
//! Two things account for that. The record is filled in place: `TextRecord`
//! wraps the `ByteRecord` the engine parses into, so reading one lays the
//! fields down once and then validates the buffer where it already lies,
//! rather than building a byte record and copying it into a second buffer.
//! And validation takes an all-ASCII fast path, folding the buffer into wide
//! words and testing their high bits together, because the general UTF-8
//! validator walks a decode state machine a byte at a time and records are
//! short enough that it never reaches its own wide path.
//!
//! Records are usually not all ASCII by luck, so the slow path matters too:
//! it validates the concatenated buffer in a single pass and then confirms
//! each field end lands on a character boundary. That second check is O(1)
//! per field and catches what a single pass cannot — a multi-byte sequence
//! split across two fields reads as valid once the delimiters between them
//! are gone. It is correctness rather than overhead, and the ASCII path skips
//! it only because every position in an ASCII buffer is a boundary.
//!
//! # What the corpus does and does not exercise
//!
//! The corpus is entirely ASCII, which is the case UTF-8 validation is fastest
//! on and the case real CSV usually is. Both sides validate with a vectorized
//! ASCII fast path, so this table compares two fast paths against each other
//! and says nothing about how either behaves on multi-byte input. A corpus of
//! accented or CJK text would be a different measurement, and a fair one, but
//! it would not be comparable to the `byte_record` table beside it.
//!
//! # What is and is not measured
//!
//! As in `byte_record`: parsers and record buffers are built in `setup` and
//! dropped in `teardown`, so neither allocation nor deallocation is counted.
//! Both sides get a record pre-sized to the corpus, because a reusable record
//! exists precisely so that steady-state reads refill it without allocating.
//!
//! # Fairness
//!
//! `TextRecord` and `csv`'s `StringRecord` are the same idea — an owned record
//! whose fields are known to be UTF-8 — reached the same way, by validating a
//! byte record. Headers are disabled on both sides, the buffer is 8 KiB where
//! one exists, and the checksum is asserted rather than assumed.
//!
//! One difference is worth naming rather than leaving to be discovered: this
//! sums `str::len`, which is a byte length, so the checksum is identical to the
//! `byte_record` table's. That is deliberate. It keeps the two tables checking
//! the same invariant, and on an ASCII corpus byte length and character count
//! agree anyway.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, IoParser, PushParser, SliceParser, TextRecord};
use gungraun::prelude::*;

#[path = "fixture.rs"]
mod fixture;

use fixture::{BUFFER, FIELDS, ROW_LEN, ROWS_1, ROWS_10, ROWS_100, ROWS_1000, check, drop_it};

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(BUFFER)
}

/// A record buffer wide enough for the corpus, so no case grows one while
/// being measured.
fn record() -> TextRecord {
    TextRecord::with_capacity(FIELDS, ROW_LEN)
}

// ── setup: everything that is not per-record work ────────────────────────────

type SliceState = (SliceParser<'static, Csv>, TextRecord, &'static [u8]);
type IoState = (
    IoParser<Cursor<&'static [u8]>, Csv>,
    TextRecord,
    &'static [u8],
);
type PushState = (PushParser<Csv>, TextRecord, &'static [u8]);
type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::StringRecord,
    &'static [u8],
);

/// One [`ByteRecord`] per row, already filled, so the `lossy` case measures
/// only [`TextRecord::from_byte_record_lossy`] and not the parse that feeds
/// it. Built with [`SliceParser`] outside the measured region.
type LossyState = (Vec<ByteRecord>, &'static [u8]);

fn slice_state(input: &'static [u8]) -> SliceState {
    let parser = SliceParser::<Csv>::new(input, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), input)
}

fn io_state(input: &'static [u8]) -> IoState {
    let parser = IoParser::<_, Csv>::new(Cursor::new(input), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), input)
}

fn push_state(input: &'static [u8]) -> PushState {
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), input)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn csv_state(input: &'static [u8]) -> CsvState {
    let reader = ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(input));
    (
        reader,
        ::csv::StringRecord::with_capacity(ROW_LEN, FIELDS),
        input,
    )
}

fn lossy_state(input: &'static [u8]) -> LossyState {
    let mut parser = SliceParser::<Csv>::new(input, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    let mut records = Vec::new();
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let mut record = ByteRecord::with_capacity(FIELDS, ROW_LEN);
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        records.push(record);
    }
    (records, input)
}

// ── the measured bodies ──────────────────────────────────────────────────────

fn sum(record: &TextRecord) -> u64 {
    let mut total = 0_u64;
    for index in 0..record.len() {
        total = total.wrapping_add(record.get(index).map_or(0, str::len) as u64);
    }
    total
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice(state: SliceState) -> (u64, SliceParser<'static, Csv>, TextRecord) {
    let (mut parser, mut record, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_text_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, input)), parser, record)
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = io_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = io_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = io_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = io_state, teardown = drop_it)]
fn io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, TextRecord) {
    let (mut parser, mut record, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_text_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, input)), parser, record)
}

// `finish` is inside the measured region because it is what tells the parser
// the unterminated tail is complete; without it the final record of a stream
// never arrives, so it is per-record work rather than teardown.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = push_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = push_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = push_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = push_state, teardown = drop_it)]
fn push(state: PushState) -> (u64, PushParser<Csv>, TextRecord) {
    let (mut parser, mut record, input) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(check(total, input)), parser, record)
}

fn drain(chunk: &mut Chunk<'_, '_, Csv>, record: &mut TextRecord) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = chunk
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_text_record_into(record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(record));
    }
    total
}

// The `csv` crate reading the same bytes into its own reusable `StringRecord`.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = csv_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = csv_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = csv_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = csv_state, teardown = drop_it)]
fn csv(
    state: CsvState,
) -> (
    u64,
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::StringRecord,
) {
    let (mut reader, mut record, input) = state;
    let mut total = 0_u64;
    while reader
        .read_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, input)), reader, record)
}

// Lossily converting an already-built `ByteRecord` to `TextRecord` over this
// corpus, which is entirely ASCII and therefore entirely valid UTF-8. This is
// the all-valid fast path P12 targets: one whole-buffer validation plus an
// O(1) char-boundary check per field end, instead of one `str::from_utf8`
// call per field.
//
// Each row's `ByteRecord` is consumed by the call and cannot be reused across
// iterations, so — unlike `slice`, `io`, `push` and `csv` above — this case
// cannot refill one buffer in a loop. `setup` instead hands over one
// already-filled `ByteRecord` per row, built outside the measured region, so
// only the conversion itself is charged.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = lossy_state)]
#[bench::rows_10(args = (ROWS_10), setup = lossy_state)]
#[bench::rows_100(args = (ROWS_100), setup = lossy_state)]
#[bench::rows_1000(args = (ROWS_1000), setup = lossy_state)]
fn lossy(state: LossyState) -> u64 {
    let (records, input) = state;
    let mut total = 0_u64;
    for record in records {
        let text = TextRecord::from_byte_record_lossy(record);
        total = total.wrapping_add(sum(&text));
    }
    black_box(check(total, input))
}

library_benchmark_group!(
    name = text_record;
    benchmarks = slice, io, push, csv, lossy
);

main!(library_benchmark_groups = text_record);
