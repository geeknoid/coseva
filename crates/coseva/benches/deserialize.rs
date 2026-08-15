//! Deserializing two of six columns into a Serde struct, next to the `csv` crate.
//!
//! Every other owned benchmark walks all six fields of a record. This one asks
//! a narrower question: given a header row, deserialize just two non-adjacent
//! columns — `name` and `population` — through Serde into a named struct, and
//! see what the field mapping plus scalar parsing costs on top of the parse.
//! Picking columns 0 and 2 rather than a leading pair is deliberate, so the
//! table reflects selecting fields out of a wider record rather than reading a
//! prefix and stopping.
//!
//! # The two groups
//!
//! `borrowed` deserializes into a struct whose `name` is a `&str` pointing into
//! the record, so nothing is allocated per record and the table isolates the
//! parse plus Serde's field dispatch and integer parsing. `owned` deserializes
//! the same two columns into a struct whose `name` is a `String`, which
//! allocates once per record on both sides. That allocation is the point of the
//! second group: allocator behavior varies with host and load far more than
//! parsing does, so reading `owned` against `borrowed` shows what owning the
//! string adds
//! rather than folding it invisibly into every number. Both groups accumulate
//! the same checksum, which is asserted, so a case cannot drift into
//! deserializing a different column and still look comparable.
//!
//! # Fairness
//!
//! All eight cases read the same `static` corpus with the same dialect, an
//! 8 KiB buffer where one exists, headers enabled on both sides, and pre-sized
//! record buffers so no case grows one while measured. Parsers and records are
//! built in `setup` and dropped in `teardown`, so neither allocation nor
//! deallocation of the machinery is counted.
//!
//! One asymmetry is real and worth naming rather than leaving to be discovered.
//! coseva's [`Line::deserialized`](coseva::Line::deserialized) deserializes
//! straight from parser storage without first materializing an owned record,
//! while `csv` has no borrowed record form: it must read into a `ByteRecord`
//! and deserialize from that. Each side is therefore taking its own natural
//! path rather than being handicapped by the benchmark, but a reader comparing
//! this table to `byte_record`'s — where `csv` also fills a `ByteRecord` —
//! deserves to know that neither group here makes coseva build an owned record.
//!
//! # The corpus
//!
//! Unlike the other three record benchmarks, this corpus carries a header row
//! and is all ASCII. The header exists because a named struct maps its fields
//! to columns by name, so both crates need one to resolve `name` and
//! `population` to columns 0 and 2. It is otherwise the shared fixture row
//! repeated, so the field bytes are identical to the tables beside it.
//!
//! One thing the table cannot be read for is the `rows_1` column. `csv` has to
//! be handed its headers, and taking them in `setup` parses the header row and
//! warms the reader before measuring starts; coseva resolves headers lazily on
//! the first `deserialized` call, which lands inside the measured region. So
//! coseva's `rows_1` carries a fixed startup cost of roughly 9,000 instructions
//! that `csv`'s does not. Differencing the last two columns cancels it, which
//! is why the per-record column is the one to compare.
//!
//! # Results
//!
//! Callgrind instruction counts. Per record is `(rows_1000 - rows_100) / 900`,
//! which cancels the fixed startup described above.
//!
//! `borrowed` — `name: &str`, nothing allocated per record:
//!
//! | Case  | 1      | 10     | 100     | 1000      | Per record | vs `csv` |
//! |-------|--------|--------|---------|-----------|------------|----------|
//! | slice |  5,009 | 13,676 |  99,800 |   960,996 |   957      | -44%     |
//! | push  |  5,666 | 15,071 | 108,575 | 1,043,571 | 1,039      | -40%     |
//! | io    |  6,029 | 15,733 | 111,743 | 1,076,073 | 1,071      | -38%     |
//! | csv   |  2,029 | 17,455 | 171,715 | 1,719,414 | 1,720      |          |
//!
//! `owned` — `name: String`, one allocation per record on both sides:
//!
//! | Case  | 1      | 10     | 100     | 1000      | Per record | vs `csv` |
//! |-------|--------|--------|---------|-----------|------------|----------|
//! | slice |  5,333 | 15,219 | 111,873 | 1,078,369 | 1,074      | -41%     |
//! | push  |  6,022 | 16,592 | 120,086 | 1,154,982 | 1,150      | -37%     |
//! | io    |  6,351 | 17,247 | 123,517 | 1,190,447 | 1,185      | -35%     |
//! | csv   |  2,129 | 18,473 | 181,913 | 1,821,412 | 1,822      |          |
//!
//! Owning the string costs 117 per record on `slice`, 111 on `push`, 114 on
//! `io` and 102 on `csv` — near enough to identical that the two groups say the
//! same thing about the parse, which is the useful part.
//!
//! These numbers are not the ones to quote when asking what
//! `#[derive(CsvDecode)]` saves over Serde. `decode` measures both paths in one
//! binary and answers that directly; instruction counts do not carry between
//! benchmark binaries, and the same coseva Serde path measured there comes to
//! 904 rather than the 957 above. `fixture.rs` records why.
//!
//! # Where the margin comes from
//!
//! The spread tracks the borrowed `read_record` table rather than the owned
//! ones, and for the same reason: no case here materializes an owned record.
//! Serde reads through a lending record, so a borrowed field points into parser
//! storage and an owned one allocates straight from it, and neither pays for an
//! intermediate copy. `csv` has no lending record to offer, so its `ByteRecord`
//! fill is unavoidable and its `csv_core` kernel is the same byte-at-a-time DFA
//! that sets the floor in every other table.
//!
//! The three coseva cases separate the way they always do. `slice` sees the
//! whole input and parses each record once on demand. `push` and `io` must
//! parse a record to know it fits in the window before anything asks for it, so
//! they carry that scan plus window bookkeeping; the deserialize itself then
//! reuses the record the scan already produced rather than parsing it again.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{Chunk, IoParser, PushParser, SliceParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this file builds its own header-bearing corpus and checksum"
)]
mod fixture;

use fixture::{BUFFER, FIELDS, ROW, ROW_LEN, drop_it};

/// The header row that names the six columns, so both crates can map struct
/// fields to columns by name.
const HEADER: &[u8] = b"name,state,population,latitude,longitude,active\n";

/// The width of [`HEADER`], the fixed prefix every corpus carries.
const HEADER_LEN: usize = HEADER.len();

/// The `name` and `population` of one [`fixture::ROW`], the two columns this
/// benchmark deserializes, summed the way [`check`] expects.
const PER_RECORD: u64 = 4_500_006;

/// The struct that borrows its string from the record, so no case in the
/// `borrowed` group allocates per record.
#[derive(serde::Deserialize)]
struct CityRef<'input> {
    name: &'input str,
    population: u64,
}

/// The struct that owns its string, so every case in the `owned` group
/// allocates a `String` per record on both sides.
#[derive(serde::Deserialize)]
struct CityOwned {
    name: String,
    population: u64,
}

/// A header row followed by `N` copies of [`fixture::ROW`], built at compile
/// time so no case pays to allocate its input.
const fn corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < HEADER_LEN {
        out[index] = HEADER[index];
        index += 1;
    }
    let mut offset = 0;
    while HEADER_LEN + offset < N {
        out[HEADER_LEN + offset] = ROW[offset % ROW_LEN];
        offset += 1;
    }
    out
}

static BUF_1: [u8; HEADER_LEN + ROW_LEN] = corpus();
static BUF_10: [u8; HEADER_LEN + ROW_LEN * 10] = corpus();
static BUF_100: [u8; HEADER_LEN + ROW_LEN * 100] = corpus();
static BUF_1000: [u8; HEADER_LEN + ROW_LEN * 1000] = corpus();

static ROWS_1: &[u8] = &BUF_1;
static ROWS_10: &[u8] = &BUF_10;
static ROWS_100: &[u8] = &BUF_100;
static ROWS_1000: &[u8] = &BUF_1000;

/// The number of data rows in `input`, excluding its header.
fn rows_in(input: &[u8]) -> u64 {
    ((input.len() - HEADER_LEN) / ROW_LEN) as u64
}

/// Assert the case deserialized the intended two columns of every record.
///
/// Asserted rather than assumed, so a case cannot quietly drift into
/// deserializing a different column and still look comparable. Both the
/// `borrowed` and `owned` groups produce this same checksum.
fn check(total: u64, rows: u64) -> u64 {
    let expected = rows * PER_RECORD;
    assert_eq!(total, expected, "benchmark deserialized the wrong fields");
    total
}

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::FirstRecord)
        .buffer_capacity(BUFFER)
}

// ── setup: everything that is not per-record work ────────────────────────────

type SliceState = (SliceParser<'static, Csv>, &'static [u8]);
type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, &'static [u8]);
type PushState = (PushParser<Csv>, &'static [u8]);
type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    ::csv::ByteRecord,
    &'static [u8],
);

fn slice_state(input: &'static [u8]) -> SliceState {
    let parser = SliceParser::<Csv>::new(input, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, input)
}

fn io_state(input: &'static [u8]) -> IoState {
    let parser = IoParser::<_, Csv>::new(Cursor::new(input), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, input)
}

fn push_state(input: &'static [u8]) -> PushState {
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, input)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
//
// The headers are cloned in setup because reading them inside the measured loop
// would both borrow-conflict with `&mut reader` and measure the wrong thing.
fn csv_state(input: &'static [u8]) -> CsvState {
    let mut reader = ::csv::ReaderBuilder::new()
        .has_headers(true)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(input));
    let headers = reader
        .byte_headers()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
        .clone();
    (
        reader,
        ::csv::ByteRecord::with_capacity(ROW_LEN, FIELDS),
        headers,
        input,
    )
}

// ── the measured bodies: borrowed ────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice_borrowed(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityRef = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = io_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = io_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = io_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = io_state, teardown = drop_it)]
fn io_borrowed(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityRef = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

// `finish` is inside the measured region because it is what tells the parser
// the unterminated tail is complete; without it the final record of a stream
// never arrives, so it is per-record work rather than teardown.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = push_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = push_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = push_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = push_state, teardown = drop_it)]
fn push_borrowed(state: PushState) -> (u64, PushParser<Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..]);
        total = total.wrapping_add(drain_borrowed(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_borrowed(&mut chunk));
    let _ = chunk.done();
    (black_box(check(total, rows_in(input))), parser)
}

fn drain_borrowed(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = chunk
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityRef = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    total
}

// The `csv` crate deserializing from its own `ByteRecord`, since it has no
// borrowed record form to deserialize straight from parser storage.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = csv_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = csv_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = csv_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = csv_state, teardown = drop_it)]
fn csv_borrowed(
    state: CsvState,
) -> (
    u64,
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    ::csv::ByteRecord,
) {
    let (mut reader, mut record, headers, input) = state;
    let mut total = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityRef = record
            .deserialize(Some(&headers))
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (
        black_box(check(total, rows_in(input))),
        reader,
        record,
        headers,
    )
}

// ── the measured bodies: owned ───────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice_owned(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityOwned = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = io_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = io_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = io_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = io_state, teardown = drop_it)]
fn io_owned(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityOwned = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

// `finish` is inside the measured region because it is what tells the parser
// the unterminated tail is complete; without it the final record of a stream
// never arrives, so it is per-record work rather than teardown.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = push_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = push_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = push_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = push_state, teardown = drop_it)]
fn push_owned(state: PushState) -> (u64, PushParser<Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..]);
        total = total.wrapping_add(drain_owned(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_owned(&mut chunk));
    let _ = chunk.done();
    (black_box(check(total, rows_in(input))), parser)
}

fn drain_owned(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = chunk
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityOwned = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    total
}

// The `csv` crate deserializing an owned struct from its own `ByteRecord`.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = csv_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = csv_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = csv_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = csv_state, teardown = drop_it)]
fn csv_owned(
    state: CsvState,
) -> (
    u64,
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    ::csv::ByteRecord,
) {
    let (mut reader, mut record, headers, input) = state;
    let mut total = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityOwned = record
            .deserialize(Some(&headers))
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (
        black_box(check(total, rows_in(input))),
        reader,
        record,
        headers,
    )
}

library_benchmark_group!(
    name = borrowed;
    benchmarks = slice_borrowed, io_borrowed, push_borrowed, csv_borrowed
);

library_benchmark_group!(
    name = owned;
    benchmarks = slice_owned, io_owned, push_owned, csv_owned
);

main!(library_benchmark_groups = borrowed, owned);
