//! The slice and push owned-byte rows of the customer matrix.
//!
//! The buffered comparison is isolated further in `matrix_byte_record_io.rs`
//! because its code generation is particularly sensitive to unrelated call
//! sites.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, PushParser, SliceParser};
use gungraun::prelude::*;

#[path = "documents.rs"]
mod documents;

use documents::{Document, check, document};

const BUFFER: usize = 8 * 1024;

type SliceState = (SliceParser<'static, Csv>, &'static Document);
type PushState = (PushParser<Csv>, &'static Document);

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::FirstRecord)
        .buffer_capacity(BUFFER)
}

fn drop_it<T>(value: T) {
    drop(value);
}

fn slice_state(name: &'static str) -> SliceState {
    let document = document(name);
    let parser = SliceParser::<Csv>::new(&document.bytes, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
}

fn push_state(name: &'static str) -> PushState {
    let document = document(name);
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, document)
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
#[bench::metrics(args = ("metrics"), setup = slice_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = slice_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = slice_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = slice_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = slice_state, teardown = drop_it)]
fn byte_record_slice(state: SliceState) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
    let (mut parser, document) = state;
    let mut record = ByteRecord::new();
    let mut total = 0_u64;
    while let Some(mut line) = bail!(parser.next_line()) {
        bail!(line.read_byte_record_into(&mut record));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, document)), parser, record)
}

fn drain(chunk: &mut Chunk<'_, '_, Csv>, record: &mut ByteRecord) -> u64 {
    let mut total = 0_u64;
    while bail!(chunk.read_byte_record_into(record)) {
        total = total.wrapping_add(sum(record));
    }
    total
}

#[library_benchmark]
#[bench::metrics(args = ("metrics"), setup = push_state, teardown = drop_it)]
#[bench::wide(args = ("wide"), setup = push_state, teardown = drop_it)]
#[bench::quoted(args = ("quoted"), setup = push_state, teardown = drop_it)]
#[bench::prose(args = ("prose"), setup = push_state, teardown = drop_it)]
#[bench::spreadsheet(args = ("spreadsheet"), setup = push_state, teardown = drop_it)]
fn byte_record_push(state: PushState) -> (u64, PushParser<Csv>, ByteRecord) {
    let (mut parser, document) = state;
    let mut record = ByteRecord::new();
    let input = &*document.bytes;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(check(total, document)), parser, record)
}

library_benchmark_group!(
    name = matrix_byte_record;
    benchmarks = byte_record_slice, byte_record_push
);

main!(library_benchmark_groups = matrix_byte_record);
