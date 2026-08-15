//! The buffered owned-byte customer comparison in a dedicated binary.
//!
//! This pair must stay isolated: adding unrelated matrix call sites changes
//! LLVM's inlining choices by about 65 instructions per record even though the
//! parser source is unchanged. The coseva case uses the fused
//! `IoParser::read_byte_record_into` API, the direct counterpart to
//! `csv::Reader::read_byte_record`.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, IoParser};
use gungraun::prelude::*;

#[path = "documents.rs"]
mod documents;

use documents::{Document, check, document};

const BUFFER: usize = 8 * 1024;

type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, &'static Document);
type CsvState = (::csv::Reader<Cursor<&'static [u8]>>, &'static Document);

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::FirstRecord)
        .buffer_capacity(BUFFER)
}

fn drop_it<T>(value: T) {
    drop(value);
}

fn io_state(name: &'static str) -> IoState {
    let document = document(name);
    let parser = IoParser::<_, Csv>::new(Cursor::new(&*document.bytes), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
}

fn csv_state(name: &'static str) -> CsvState {
    let document = document(name);
    let reader = ::csv::ReaderBuilder::new()
        .has_headers(true)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(&*document.bytes));
    (reader, document)
}

macro_rules! bail {
    ($result:expr) => {
        $result.unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    };
}

fn sum(record: &ByteRecord) -> u64 {
    let mut total = 0_u64;
    for field in record {
        total = total.wrapping_add(field.len() as u64);
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = io_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = io_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = io_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = io_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = io_state, teardown = drop_it)]
fn byte_record_io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord) {
    let (mut parser, document) = state;
    let mut record = ByteRecord::new();
    let mut total = 0_u64;
    while bail!(parser.read_byte_record_into(&mut record)) {
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, document)), parser, record)
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = csv_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = csv_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = csv_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = csv_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = csv_state, teardown = drop_it)]
fn byte_record_csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>) {
    let (mut reader, document) = state;
    let mut record = ::csv::ByteRecord::new();
    let mut total = 0_u64;
    while bail!(reader.read_byte_record(&mut record)) {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, document)), reader)
}

library_benchmark_group!(
    name = matrix_byte_record_io;
    benchmarks = byte_record_io, byte_record_csv
);

main!(library_benchmark_groups = matrix_byte_record_io);
