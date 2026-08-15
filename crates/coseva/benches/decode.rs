//! Decoding two of six columns into a typed struct, next to the `csv` crate.
//!
//! This is the [`deserialize`](../deserialize/index.html) table with coseva's
//! side switched from Serde to its own `#[derive(CsvDecode)]`. The question is
//! the same — given a header row, turn two non-adjacent columns, `name` and
//! `population`, into a named struct — and the corpus, the checksum and the
//! four sizes are identical.
//!
//! What coseva's native decoding saves over routing the same work through
//! Serde is answered here rather than by reading the two tables against each
//! other, because instruction counts do not carry between benchmark binaries.
//! A `slice (Serde)` row runs coseva's Serde path in this file, beside the
//! derive, over the same bytes.
//!
//! # What is actually being compared
//!
//! This pairing is asymmetric by necessity and the asymmetry is the point
//! rather than a flaw to be corrected: `csv` has no typed decoding of its own,
//! and coseva's `CsvDecode` has no counterpart there. So each crate is measured
//! on its own shortest path from bytes to a typed struct, which is what a
//! caller who wants a struct would actually write. It is not a comparison of
//! two implementations of one mechanism, and reading it as one would be wrong:
//! `csv`'s column here is the same number as its column in `deserialize`,
//! because it is running the same code.
//!
//! What that buys is a fair answer to "how expensive is a row of this shape in
//! each crate". The second, more useful reading — coseva against itself with
//! only the decoding layer changed — is the `slice` and `slice (Serde)` pair
//! within each table.
//!
//! # The two groups
//!
//! `borrowed` decodes into a struct whose `name` is a `&str` pointing into the
//! record, so nothing is allocated per record. `owned` decodes the same two
//! columns into a struct whose `name` is a `String`, which allocates once per
//! record on both sides — allocator behavior varies with host and load far
//! more than parsing does, which is why that group is kept separate rather
//! than folded in. Both groups accumulate the same checksum, which is asserted, so a
//! case cannot drift into decoding a different column and still look
//! comparable.
//!
//! `owned` also carries `slice_compact`, which is `slice_owned` with a
//! `CompactString` in place of the `String`. The fixture's `name` is
//! `"Boston"`, six bytes, so it stays inline and the allocation disappears:
//! 686,932 against 770,931 instructions over 1000 rows, about 84 per record.
//! That is the price of one short-string allocation, and it is why
//! `CompactString` is worth having for the many CSV columns — state codes,
//! currencies, flags, city names — that fit inline.
//!
//! # Fairness
//!
//! All nine cases read the same `static` corpus with the same dialect, an
//! 8 KiB buffer where one exists, headers enabled on both sides, and pre-sized
//! record buffers so no case grows one while measured. Parsers and records are
//! built in `setup` and dropped in `teardown`, so neither allocation nor
//! deallocation of the machinery is counted.
//!
//! The record-form asymmetry from `deserialize` applies here unchanged.
//! coseva's [`Line::decoded`](coseva::Line::decoded) decodes straight from
//! parser storage without materializing an owned record; `csv` has no borrowed
//! record form and must fill a `ByteRecord` first. Each side takes its natural
//! path, but a reader comparing this to `byte_record`'s table deserves to know
//! that no group here makes coseva build an owned record.
//!
//! # The corpus
//!
//! A header row followed by copies of the shared fixture row, all ASCII,
//! identical to `deserialize`'s. The header exists because a named struct maps
//! its fields to columns by name, so both crates need one to resolve `name` and
//! `population` to columns 0 and 2.
//!
//! `rows_1` is not comparable across crates, for the same reason it is not in
//! `deserialize`: `csv` is handed its headers in `setup`, which parses the
//! header row and warms the reader before measuring starts, while coseva
//! resolves headers lazily inside the first measured call. Differencing the
//! last two columns cancels it, which is why the per-record column is the one
//! to compare.
//!
//! # Results
//!
//! Callgrind instruction counts. Per record is `(rows_1000 - rows_100) / 900`,
//! which cancels the fixed startup described above.
//!
//! `borrowed` — `name: &str`, nothing allocated per record:
//!
//! | Case          | 1     | 10     | 100     | 1000      | Per record | vs `csv` |
//! |---------------|-------|--------|---------|-----------|------------|----------|
//! | slice         | 5,259 | 11,238 |  70,365 |   661,553 |   657      | -62%     |
//! | push          | 5,916 | 12,633 |  79,140 |   744,128 |   739      | -57%     |
//! | io            | 6,303 | 13,292 |  82,303 |   776,530 |   771      | -55%     |
//! | slice (Serde) | 4,969 | 13,159 |  94,513 |   908,009 |   904      | -47%     |
//! | csv           | 2,029 | 17,455 | 171,715 | 1,719,414 | 1,720      |          |
//!
//! `owned` — `name: String`, one allocation per record on both sides:
//!
//! | Case          | 1     | 10     | 100     | 1000      | Per record | vs `csv` |
//! |---------------|-------|--------|---------|-----------|------------|----------|
//! | slice         | 5,536 | 12,523 |  81,730 |   773,718 |   769      | -58%     |
//! | push          | 6,225 | 13,896 |  89,943 |   850,331 |   845      | -54%     |
//! | io            | 6,578 | 14,548 |  93,369 |   885,696 |   880      | -52%     |
//! | slice (Serde) | 5,273 | 14,520 | 104,784 | 1,007,380 | 1,003      | -45%     |
//! | csv           | 2,129 | 18,473 | 181,913 | 1,821,412 | 1,822      |          |
//!
//! # Where the margin comes from
//!
//! The `slice (Serde)` row is coseva's own Serde path over the same corpus in
//! the same binary, and it is there because instruction counts do not carry
//! between benchmark binaries — `fixture.rs` records the measurement that
//! shows why. It differs from `slice` only in the decoding layer, so the two
//! rows subtract cleanly: native decoding costs 657 against Serde's 904
//! borrowed, a saving of 27%, and 769 against 1,003 owned, a saving of 23%.
//!
//! A target naming a subset of the columns resolves to the same mapping and the
//! same vectorized kernel as one naming all of them. Materializing only the
//! named columns through a dedicated scalar kernel sounds cheaper and is not,
//! once the
//! record lends rather than copies: there is no per-column copy left to skip,
//! so such a kernel buys nothing and pays for it by not being vectorized. On
//! `push` and `io` it is worse still, because those front ends must parse a
//! record to know it fits the window, and a projected branch would then discard
//! that parse and rescan.
//!
//! The mechanism behind the margin is that `CsvDecode` reads fields by
//! position through a mapping resolved once against the header, with no
//! `Deserializer` dispatch and no visitor between the field bytes and the
//! target's parse.
//!
//! `rows_1` favors `csv` in both groups because its reader is warmed in
//! `setup`, where coseva resolves headers and its column mapping inside the
//! first measured call. It is also the one column where the derive does not
//! win: 5,259 against Serde's 4,969 borrowed. That inverts because the derive
//! resolves its column mapping on the first call, and at one record there is
//! nothing to amortize it over. Differencing the last two columns removes all
//! of this, which is why the per-record column is the one to compare.
//!
//! Both tables share the `csv` column, so the margin over `csv` moves for only
//! one reason: coseva got cheaper. The three coseva cases separate the way they
//! always do — `slice` sees the whole input and parses each record once on
//! demand, while `push` and `io` must parse a record to know it fits in the
//! window before anything asks for it, and carry that scan plus window
//! bookkeeping.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use compact_str::CompactString;
use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
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
/// benchmark decodes, summed the way [`check`] expects.
const PER_RECORD: u64 = 4_500_006;

/// The struct that borrows its string from the record, so no case in the
/// `borrowed` group allocates per record.
#[derive(CsvDecode)]
struct CityRef<'input> {
    name: &'input str,
    population: u64,
}

/// The struct that owns its string, so every case in the `owned` group
/// allocates a `String` per record on both sides.
#[derive(CsvDecode)]
struct CityOwned {
    name: String,
    population: u64,
}

/// The same shape as [`CityOwned`], but with the string held inline. `name` is
/// `"Boston"`, six bytes, so it never reaches the allocator — which is the
/// whole of the difference this case is here to price.
#[derive(CsvDecode)]
struct CityCompact {
    name: CompactString,
    population: u64,
}

/// The Serde targets, used both by the `csv` crate — which has no typed
/// decoding of its own — and by coseva's own Serde cases, so that the derive
/// and Serde are decoding structurally identical shapes.
#[derive(serde::Deserialize)]
struct CsvCityRef<'input> {
    name: &'input str,
    population: u64,
}

#[derive(serde::Deserialize)]
struct CsvCityOwned {
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

/// Assert the case decoded the intended two columns of every record.
///
/// Asserted rather than assumed, so a case cannot quietly drift into decoding a
/// different column and still look comparable. Both the `borrowed` and `owned`
/// groups produce this same checksum, which is also `deserialize`'s.
fn check(total: u64, rows: u64) -> u64 {
    let expected = rows * PER_RECORD;
    assert_eq!(total, expected, "benchmark decoded the wrong fields");
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
            .decoded()
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
            .decoded()
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
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    total
}

// coseva's own Serde path, measured here rather than read across from
// `deserialize`, because instruction counts are not comparable between binaries
// (see `fixture.rs`). This row and `slice_borrowed` differ only in the decoding
// layer, so their difference is the saving the derive actually buys.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice_serde_borrowed(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CsvCityRef = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

// The `csv` crate deserializing from its own `ByteRecord`, since it has neither
// a typed decoding path nor a borrowed record form.
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
        let city: CsvCityRef = record
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
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice_compact(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    // `slice_owned` with a `CompactString` in place of the `String`. Same front
    // end, same corpus, same checksum: the delta is the allocation.
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CityCompact = line
            .decoded()
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
            .decoded()
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
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    total
}

// coseva's own Serde path for the owned shape, the counterpart to
// `slice_serde_borrowed`.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice_serde_owned(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let city: CsvCityOwned = line
            .deserialized()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(city.name.len() as u64 + city.population);
    }
    (black_box(check(total, rows_in(input))), parser)
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
        let city: CsvCityOwned = record
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
    benchmarks = slice_borrowed, io_borrowed, push_borrowed, slice_serde_borrowed,
        csv_borrowed
);

library_benchmark_group!(
    name = owned;
    benchmarks = slice_owned, slice_compact, io_owned, push_owned, slice_serde_owned,
        csv_owned
);

main!(library_benchmark_groups = borrowed, owned);
