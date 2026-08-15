//! The customer matrix: five record shapes across three front ends, over five
//! documents that look like files people actually have.
//!
//! Every other suite in this directory answers a question about this crate's
//! internals — what a header lookup costs, what quoting costs, what the
//! structural scan costs. This one answers the only question a person choosing
//! a CSV parser actually asks: *for a file like mine, read the way I intend to
//! read it, what does this cost, and what does the alternative cost?*
//!
//! It is rendered into [`docs/PERF.md`](../../docs/PERF.md) by
//! `scripts/perf_report.rs`, which reads this suite plus the isolated owned-byte
//! suites. Nothing in that file is transcribed by hand.
//!
//! # The matrix
//!
//! Five ways to get at a record, each across `slice`, `io` and `push`:
//!
//! | Shape          | What it gives you                       | `csv` counterpart |
//! |----------------|-----------------------------------------|-------------------|
//! | `record`       | fields borrowed from the input          | none              |
//! | `text_record`  | an owned record of `String`s            | `StringRecord`    |
//! | `byte_record`  | an owned record of `Vec<u8>`s           | `ByteRecord`      |
//! | `decoded`      | a struct, via `#[derive(CsvDecode)]`     | none              |
//! | `deserialized` | a struct, via Serde                     | Serde             |
//!
//! Two of the five have no `csv` counterpart at all. `csv` has no borrowed
//! record form — its `StringRecord` always owns — and no typed decoding of its
//! own beyond Serde. Those rows are published anyway, labelled as having no
//! comparison, because a capability the alternative does not have is a result
//! and hiding it would make the table look narrower than the crate is. What
//! would not be honest is printing them beside a `csv` number they did not
//! race against, which is why the column says so explicitly.
//!
//! # The documents
//!
//! Generated, not scraped; see [`documents`] for what each shape stresses and
//! why generated data is the right call for a benchmark. All five start with
//! the same `name` and `value` columns, so the typed rows decode one schema
//! everywhere and their differences are the document rather than the struct.
//!
//! # Reading these numbers
//!
//! Instruction counts, not time. They are reproducible to the instruction,
//! which is what makes a 2% change meaningful, but they do not model cache
//! behaviour or branch prediction, and a document large enough to fall out of
//! L2 will not be modelled well by any of them. Treat them as a strong signal
//! about work done and a weak one about wall clock.
//!
//! Every case asserts a checksum computed by the generator rather than by a
//! parse, so a case that skipped a column or stopped unescaping fails instead
//! of reporting a better number. `tests/benchmark_parity.rs` runs the same
//! assertions without valgrind.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::{Chunk, IoParser, PushParser, SliceParser, TextRecord};
use gungraun::prelude::*;

#[path = "documents.rs"]
mod documents;

use documents::{Document, check, check_values, document};

/// The buffer both crates are given, so no comparison becomes a comparison of
/// buffer sizes. Matches `csv`'s own default.
const BUFFER: usize = 8 * 1024;

/// The two columns every typed row decodes, present in all five documents.
#[derive(CsvDecode)]
struct Row {
    value: u64,
}

#[derive(serde::Deserialize)]
struct SerdeRow {
    value: u64,
}

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::FirstRecord)
        .buffer_capacity(BUFFER)
}

fn drop_it<T>(value: T) {
    drop(value);
}

// ── setup, which is outside every measured region ────────────────────────────

type SliceState = (SliceParser<'static, Csv>, &'static Document);
type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, &'static Document);
type PushState = (PushParser<Csv>, &'static Document);
type CsvState = (::csv::Reader<Cursor<&'static [u8]>>, &'static Document);

fn slice_state(name: &'static str) -> SliceState {
    let document = document(name);
    let parser = SliceParser::<Csv>::new(&document.bytes, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
}

fn io_state(name: &'static str) -> IoState {
    let document = document(name);
    let parser = IoParser::<_, Csv>::new(Cursor::new(&*document.bytes), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
}

fn push_state(name: &'static str) -> PushState {
    let document = document(name);
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn csv_state(name: &'static str) -> CsvState {
    let document = document(name);
    let reader = ::csv::ReaderBuilder::new()
        .has_headers(true)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(&*document.bytes));
    (reader, document)
}

// ── the measured bodies ──────────────────────────────────────────────────────

macro_rules! bail {
    ($result:expr) => {
        $result.unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    };
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = slice_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = slice_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = slice_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = slice_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = slice_state, teardown = drop_it)]
fn record_slice(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let record = bail!(line.record());
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = io_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = io_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = io_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = io_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = io_state, teardown = drop_it)]
fn record_io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let record = bail!(line.record());
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, document)), parser)
}

fn drain_record(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = bail!(chunk.next_line()) {
        let record = bail!(line.record());
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = push_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = push_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = push_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = push_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = push_state, teardown = drop_it)]
fn record_push(state: PushState) -> (u64, PushParser<Csv>) {
    let (mut parser, document) = state;
    let input = &*document.bytes;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain_record(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_record(&mut chunk));
    let _ = chunk.done();
    (black_box(check(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = slice_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = slice_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = slice_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = slice_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = slice_state, teardown = drop_it)]
fn text_record_slice(state: SliceState) -> (u64, SliceParser<'static, Csv>, TextRecord) {
    let (mut parser, document) = state;
    let mut record = TextRecord::new();
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        bail!(line.read_text_record_into(&mut record));
        for index in 0..record.len() {
            total = total.wrapping_add(record.get(index).map_or(0, str::len) as u64);
        }
    }
    (black_box(check(total, document)), parser, record)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = io_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = io_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = io_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = io_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = io_state, teardown = drop_it)]
fn text_record_io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, TextRecord) {
    let (mut parser, document) = state;
    let mut record = TextRecord::new();
    let mut total = 0_u64;
    while bail!(parser.read_text_record_into(&mut record)) {
        for index in 0..record.len() {
            total = total.wrapping_add(record.get(index).map_or(0, str::len) as u64);
        }
    }
    (black_box(check(total, document)), parser, record)
}

fn drain_text_record(chunk: &mut Chunk<'_, '_, Csv>, record: &mut TextRecord) -> u64 {
    let mut total = 0_u64;
    while bail!(chunk.read_text_record_into(record)) {
        for index in 0..record.len() {
            total = total.wrapping_add(record.get(index).map_or(0, str::len) as u64);
        }
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = push_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = push_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = push_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = push_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = push_state, teardown = drop_it)]
fn text_record_push(state: PushState) -> (u64, PushParser<Csv>, TextRecord) {
    let (mut parser, document) = state;
    let mut record = TextRecord::new();
    let input = &*document.bytes;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain_text_record(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_text_record(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(check(total, document)), parser, record)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = slice_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = slice_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = slice_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = slice_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = slice_state, teardown = drop_it)]
fn decoded_slice(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let row: Row = bail!(line.decoded());
        total = total.wrapping_add(row.value);
    }
    (black_box(check_values(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = io_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = io_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = io_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = io_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = io_state, teardown = drop_it)]
fn decoded_io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let row: Row = bail!(line.decoded());
        total = total.wrapping_add(row.value);
    }
    (black_box(check_values(total, document)), parser)
}

fn drain_decoded(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = bail!(chunk.next_line()) {
        let row: Row = bail!(line.decoded());
        total = total.wrapping_add(row.value);
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = push_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = push_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = push_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = push_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = push_state, teardown = drop_it)]
fn decoded_push(state: PushState) -> (u64, PushParser<Csv>) {
    let (mut parser, document) = state;
    let input = &*document.bytes;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain_decoded(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_decoded(&mut chunk));
    let _ = chunk.done();
    (black_box(check_values(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = slice_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = slice_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = slice_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = slice_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = slice_state, teardown = drop_it)]
fn deserialized_slice(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let row: SerdeRow = bail!(line.deserialized());
        total = total.wrapping_add(row.value);
    }
    (black_box(check_values(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = io_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = io_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = io_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = io_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = io_state, teardown = drop_it)]
fn deserialized_io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>) {
    let (mut parser, document) = state;
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        let row: SerdeRow = bail!(line.deserialized());
        total = total.wrapping_add(row.value);
    }
    (black_box(check_values(total, document)), parser)
}

fn drain_deserialized(chunk: &mut Chunk<'_, '_, Csv>) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = bail!(chunk.next_line()) {
        let row: SerdeRow = bail!(line.deserialized());
        total = total.wrapping_add(row.value);
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = push_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = push_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = push_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = push_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = push_state, teardown = drop_it)]
fn deserialized_push(state: PushState) -> (u64, PushParser<Csv>) {
    let (mut parser, document) = state;
    let input = &*document.bytes;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain_deserialized(&mut chunk));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain_deserialized(&mut chunk));
    let _ = chunk.done();
    (black_box(check_values(total, document)), parser)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = csv_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = csv_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = csv_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = csv_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = csv_state, teardown = drop_it)]
fn text_record_csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>) {
    let (mut reader, document) = state;
    let mut record = ::csv::StringRecord::new();
    let mut total = 0_u64;
    while bail!(reader.read_record(&mut record)) {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, document)), reader)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = csv_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = csv_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = csv_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = csv_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = csv_state, teardown = drop_it)]
fn deserialized_csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>) {
    let (mut reader, document) = state;
    let mut total = 0_u64;
    let mut iter = reader.deserialize::<SerdeRow>();
    for row in &mut iter {
        total = total.wrapping_add(bail!(row).value);
    }
    (black_box(check_values(total, document)), reader)
}

library_benchmark_group!(
    name = matrix;
    benchmarks =
        record_slice,
        record_io,
        record_push,
        text_record_slice,
        text_record_io,
        text_record_push,
        decoded_slice,
        decoded_io,
        decoded_push,
        deserialized_slice,
        deserialized_io,
        deserialized_push,
        text_record_csv,
        deserialized_csv
);

main!(library_benchmark_groups = matrix);
