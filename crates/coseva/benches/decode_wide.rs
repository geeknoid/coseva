//! Decoding five of a hundred columns into a typed struct, next to `csv`.
//!
//! [`decode`](../decode/index.html) asks how expensive a six-column row is
//! when a target names two of them. This asks the same question at the width
//! where the columns nobody wants dominate the record: a hundred columns, of
//! which the target names five. It is the shape a projected parse would be
//! written to serve, so it is the shape that has to justify its absence.
//!
//! # What is actually being compared
//!
//! The asymmetry is `decode`'s, unchanged: `csv` has no typed decoding of its
//! own, so its two cases go through Serde, whose derive ignores the ninety-five
//! columns the target does not name. Each crate takes its own shortest path
//! from bytes to a struct. It is not two implementations of one mechanism.
//!
//! # The corpus
//!
//! A hundred columns named `c00` through `c99`, each holding a distinct
//! five-digit number, so a record is exactly 600 bytes and every column is the
//! same width — a width distribution would otherwise be a second variable
//! moving alongside the column count. The target names `c03`, `c27`, `c51`,
//! `c76` and `c98`: five columns spread across the record so that no prefix of
//! it contains all of them and nothing can finish early.
//!
//! Fields are deliberately plain — unquoted, no escapes, ASCII digits. That
//! flatters both crates equally and keeps the measurement about column count
//! rather than about quote handling.
//!
//! # The two groups
//!
//! `borrowed` decodes `c03` as a `&str` pointing into the record; `owned`
//! decodes it as a `String`, allocating once per record on both sides. The four
//! integers are the same in both, so the difference between the groups is one
//! allocation and nothing else.
//!
//! # Results
//!
//! Callgrind instruction counts. Per record is `(rows_1000 - rows_100) / 900`,
//! which cancels the fixed startup discussed below.
//!
//! `borrowed` — `c03: &str`, nothing allocated per record:
//!
//! | Case  | 1       | 10      | 100       | 1000       | Per record | vs `csv` |
//! |-------|---------|---------|-----------|------------|------------|----------|
//! | slice |  38,305 |  77,251 |   467,156 |  4,365,731 |   4,332    | -77%     |
//! | push  |  38,997 |  78,663 |   475,768 |  4,446,343 |   4,412    | -76%     |
//! | io    |  39,894 |  80,253 |   512,970 |  4,810,012 |   4,774    | -74%     |
//! | csv   |  18,713 | 184,295 | 1,846,254 | 18,466,626 |  18,467    |          |
//!
//! `owned` — `c03: String`, one allocation per record on both sides:
//!
//! | Case  | 1       | 10      | 100       | 1000       | Per record | vs `csv` |
//! |-------|---------|---------|-----------|------------|------------|----------|
//! | slice |  38,420 |  78,428 |   478,953 |  4,483,728 |   4,450    | -76%     |
//! | push  |  39,103 |  79,741 |   486,566 |  4,554,341 |   4,520    | -76%     |
//! | io    |  40,008 |  81,420 |   524,667 |  4,927,009 |   4,891    | -74%     |
//! | csv   |  18,820 | 185,365 | 1,856,954 | 18,573,626 |  18,574    |          |
//!
//! # The margin widens with width
//!
//! `decode`'s six-column row put `slice` 62% under `csv`, a factor of 2.6. At a
//! hundred columns it is 77% under, a factor of 4.3. The margin grows because
//! the per-column cost differs more than the per-record cost does.
//!
//! Reading the two benchmarks as two points on a line — per record equals a
//! fixed cost plus a per-column cost — gives roughly 39 instructions per column
//! for `slice` against 178 for `csv`, on fixed costs of about 420 and 650. This
//! was indicative rather than measured: the two corpora do not have the same
//! field contents, only the same shape, so some of the difference between the
//! points is field width and type rather than column count, and two points
//! cannot show curvature.
//!
//! [`width_sweep`](../width_sweep/index.html) now measures this properly, over
//! four widths whose field content is identical column for column. It finds
//! 37.0 instructions per column for `slice` against 170.8 for `csv`, both
//! strictly linear. The estimate above was high by a few percent but had the
//! direction and the size of the gap right, which is what the growing margin
//! reflects.
//!
//! Two secondary readings fall out of the tables and are worth stating because
//! they are consistency checks rather than claims. The `owned` group costs
//! about 115 instructions more per record than `borrowed` at this width, and
//! about 112 more in `decode` — one `String` allocation, unchanged by column
//! count, as it should be. And `io`'s premium over `slice` falls from 17% in
//! `decode` to 10% here: window bookkeeping is per record, not per column, so
//! it dilutes as records widen.
//!
//! Both of those read one file's table against another's, which `fixture.rs`
//! explains is not safe for absolute counts. They are stated as ratios within
//! each file for that reason, and offered as consistency checks rather than
//! measurements.
//!
//! # Why projection would not pay here
//!
//! Ninety-five of a hundred columns are discovered and never read, which is the
//! regime a projected kernel would be written for. The tables say it
//! would not pay, for the reason `docs/DESIGN.md` gives: a discovered
//! field costs one `Span` push, and the scan has to cross its bytes either way
//! to find where the next one begins. Skipping the push saves the push. It does
//! not save the crossing, and it costs the vectorized scan that does the
//! crossing quickly.
//!
//! This benchmark measures no projected kernel — it establishes only that
//! coseva's margin over `csv` grows rather
//! than shrinks in the regime where such a kernel would be expected to matter.
//!
//! # `rows_1` and the cost of a hundred headers
//!
//! `rows_1` is not a comparison and should not be read as one: `csv` parses its
//! header row in `setup`, outside the measured region, where coseva resolves
//! headers inside the first measured call. But coseva's column is worth
//! watching, because it is where a per-column setup cost shows up undiluted.
//!
//! `rebuild_header_lookup` builds a `HashMap` over all hundred
//! header names — a map that neither typed decode nor the Serde path ever
//! reads, since both resolve their columns by scanning the header record
//! directly — and it costs about 138,000 instructions here. It is built on
//! demand, so a run that only decodes never builds
//! it at all, and the column reads 38,305 rather than 142,873. No per-record
//! column differs by a single instruction between the two, which is the point:
//! it is all setup.
//!
//! What is left is real work — parsing the hundred-column header record, and
//! `resolve_typed_mapping` scanning it once per named column. That scan is
//! `O(names x columns)`, and this corpus is close to its worst case, since
//! every header is exactly as long as every name and so no length check can
//! reject a candidate before the byte compare. It still costs a fifth of what
//! the map did, which is why looking columns up by scanning is the right
//! default and the map is the exception.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]
#![expect(
    clippy::cast_possible_truncation,
    reason = "the corpus builder's digits are one decimal place by construction"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::{Chunk, IoParser, PushParser, SliceParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this file builds its own wide corpus and checksum"
)]
mod fixture;

use fixture::{BUFFER, drop_it};

/// The number of columns every record carries.
const COLUMNS: usize = 100;

/// The width of a column name, `c00` through `c99`.
const NAME_LEN: usize = 3;

/// The width of every field value, so a record is a fixed size and the corpus
/// can be built and differenced without measuring a length distribution.
const VALUE_LEN: usize = 5;

/// The width of the header row, one name and one delimiter per column.
const HEADER_LEN: usize = COLUMNS * (NAME_LEN + 1);

/// The width of a data row, one value and one delimiter per column.
const ROW_LEN: usize = COLUMNS * (VALUE_LEN + 1);

/// The value column `index` carries, chosen so every column holds a distinct
/// five-digit number and no selected column can be confused with another.
const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

/// The five columns the target names, spread across the width so that no
/// prefix of the record contains all of them.
const SELECTED: [usize; 5] = [3, 27, 51, 76, 98];

/// The checksum one record contributes: the length of the borrowed `c03` plus
/// the four parsed integers.
const PER_RECORD: u64 = VALUE_LEN as u64
    + value(SELECTED[1])
    + value(SELECTED[2])
    + value(SELECTED[3])
    + value(SELECTED[4]);

const fn header() -> [u8; HEADER_LEN] {
    let mut out = [0_u8; HEADER_LEN];
    let mut index = 0;
    while index < COLUMNS {
        let base = index * (NAME_LEN + 1);
        out[base] = b'c';
        out[base + 1] = b'0' + (index / 10) as u8;
        out[base + 2] = b'0' + (index % 10) as u8;
        out[base + 3] = if index + 1 == COLUMNS { b'\n' } else { b',' };
        index += 1;
    }
    out
}

const fn row() -> [u8; ROW_LEN] {
    let mut out = [0_u8; ROW_LEN];
    let mut index = 0;
    while index < COLUMNS {
        let base = index * (VALUE_LEN + 1);
        let mut remaining = value(index);
        let mut digit = VALUE_LEN;
        while digit > 0 {
            digit -= 1;
            out[base + digit] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
        }
        out[base + VALUE_LEN] = if index + 1 == COLUMNS { b'\n' } else { b',' };
        index += 1;
    }
    out
}

static HEADER: [u8; HEADER_LEN] = header();
static ROW: [u8; ROW_LEN] = row();

/// A header row followed by `N` copies of [`ROW`], built at compile time so no
/// case pays to allocate its input.
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

/// The struct that borrows `c03` from the record, so no case in the `borrowed`
/// group allocates per record.
#[derive(CsvDecode)]
struct WideRef<'input> {
    c03: &'input str,
    c27: u64,
    c51: u64,
    c76: u64,
    c98: u64,
}

/// The struct that owns `c03`, so every case in the `owned` group allocates a
/// `String` per record on both sides.
#[derive(CsvDecode)]
struct WideOwned {
    c03: String,
    c27: u64,
    c51: u64,
    c76: u64,
    c98: u64,
}

/// The `csv` crate has no typed decoding, so its two cases go through Serde.
/// Serde's derive ignores the ninety-five columns these do not name.
#[derive(serde::Deserialize)]
struct CsvWideRef<'input> {
    c03: &'input str,
    c27: u64,
    c51: u64,
    c76: u64,
    c98: u64,
}

#[derive(serde::Deserialize)]
struct CsvWideOwned {
    c03: String,
    c27: u64,
    c51: u64,
    c76: u64,
    c98: u64,
}

/// The number of data rows in `input`, excluding its header.
fn rows_in(input: &[u8]) -> u64 {
    ((input.len() - HEADER_LEN) / ROW_LEN) as u64
}

/// Assert the case decoded the intended five columns of every record.
///
/// Asserted rather than assumed, so a case that silently read the wrong column
/// — or a mapping that resolved to the wrong offsets — cannot still look
/// comparable. Both groups produce this same checksum.
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
        ::csv::ByteRecord::with_capacity(ROW_LEN, COLUMNS),
        headers,
        input,
    )
}

/// The checksum contribution of one decoded row, written once so the eight
/// cases cannot drift apart in what they charge themselves for.
fn fold_ref(row: &WideRef<'_>) -> u64 {
    row.c03.len() as u64 + row.c27 + row.c51 + row.c76 + row.c98
}

fn fold_owned(row: &WideOwned) -> u64 {
    row.c03.len() as u64 + row.c27 + row.c51 + row.c76 + row.c98
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
        let row: WideRef = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_ref(&row));
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
        let row: WideRef = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_ref(&row));
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
        let row: WideRef = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_ref(&row));
    }
    total
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
        let row: CsvWideRef = record
            .deserialize(Some(&headers))
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(row.c03.len() as u64 + row.c27 + row.c51 + row.c76 + row.c98);
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
        let row: WideOwned = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_owned(&row));
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
        let row: WideOwned = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_owned(&row));
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
        let row: WideOwned = line
            .decoded()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(fold_owned(&row));
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
        let row: CsvWideOwned = record
            .deserialize(Some(&headers))
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(row.c03.len() as u64 + row.c27 + row.c51 + row.c76 + row.c98);
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
