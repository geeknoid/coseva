//! Reading records into a reusable owned [`ByteRecord`], next to the `csv` crate.
//!
//! This is the owned-record counterpart to `read_record`, which measures the
//! borrowed path. Here every case refills one record buffer per iteration and
//! walks its fields, so the field bytes are copied out of the input in all four
//! columns and the comparison is like for like.
//!
//! | Case    |    1 |    10 |    100 |   1000 | Per record |
//! |---------|------|-------|--------|--------|------------|
//! | `slice` | 1131 |  8321 |  79494 | 791785 |        791 |
//! | `push`  | 1845 | 12582 |  98619 | 959820 |        957 |
//! | `io`    | 1646 | 12545 |  99765 | 978024 |        976 |
//! | `csv`   | 2686 | 12372 | 109014 | 1076471 |      1075 |
//!
//! Every coseva front end is ahead of `csv` here: 791 against 1075 for
//! `slice`, and 957 and 976 for `push` and `io`, which are the front ends that
//! buffer the way `csv` does and so are its closer peers.
//!
//! That the borrowed table's ordering survives into this one is the thing to
//! notice, but it survives weakened. Materializing six fields into an owned
//! record costs `slice` 336 instructions per record on top of the 455 it
//! spends parsing them. Around 86 of that is the copy itself, and the rest is
//! the owned kernel's bookkeeping: appending fields and maintaining the record
//! buffers. Any caller who does not need an owned record should not ask for
//! one, and the gap between this table and `read_record`'s is the price of
//! asking.
//!
//! The comparison is not shaped the same on both sides, and it is worth being
//! exact about how, because the naive reading of the paragraph above is that
//! `csv` somehow avoids this work. It does not. Both sides hand back an owned
//! record, both copy the field bytes into it, and with the record pre-sized
//! neither allocates in steady state.
//!
//! What differs is where the copy lives. `csv_core` is a byte-at-a-time state
//! machine that writes each byte into the caller's buffer as it classifies it,
//! so its copy is spread through the 845 instructions per record it charges
//! for parsing and never appears as a symbol of its own. coseva recognizes a
//! field as a contiguous run and copies it in one go, which is why
//! `__memcpy_avx_unaligned_erms` shows up in this profile at 86 instructions
//! per record against `csv`'s 5. The work is the same; only its attribution
//! differs.
//!
//! That 86 is also the number in this table to trust least. Valgrind charges
//! an ERMS copy one instruction per byte where the hardware moves thirty-two
//! at a time, so coseva's copy is overstated here while `csv`'s per-byte
//! writes are charged at something much closer to their true cost.
//!
//! So the reason coseva wins the borrowed table by a wider margin than this
//! one is not that its owned path is bloated. It is that `csv` has only one
//! mode: producing an owned record costs it 845 instructions in the kernel and
//! producing a borrowed one is not something it offers at all, so 845 is its
//! floor. coseva charges 354 in the kernel for a borrowed record and 622 for
//! an owned one. The margin narrows here because our own floor rose, not
//! because `csv` did anything better.
//!
//! # Why `csv` and not `csv-core`
//!
//! `read_record` compares against `csv-core`, which is the right peer for a
//! borrowed record: it is a raw state machine with no allocation and no record
//! type. It is the wrong peer for an owned one, and it is the wrong peer for
//! `IoParser` in particular, which reads from a source and owns a window.
//! Comparing a buffered reader against a state machine that never reads
//! anything measures the buffering, not the parsing.
//!
//! The `csv` crate is the honest comparison for this shape. It buffers, it
//! reads, it hands back an owned `ByteRecord`, and it is what a caller reaching
//! for a CSV crate actually reaches for. That is why `io` appears in this table
//! and not in `read_record`'s.
//!
//! # What is and is not measured
//!
//! Constructing a parser allocates its buffers and resolves its dialect, and
//! dropping one frees them. Neither is per-record work, so `setup` builds the
//! parser before Callgrind starts counting and the benchmark hands it back for
//! `teardown` to drop after counting stops.
//!
//! The record buffer is allocated in `setup` too, on both sides and to the same
//! field and byte capacity. This is the point of a reusable record: the whole
//! design is that steady-state reads refill it in place without allocating, so
//! charging one case for an allocation the other made earlier would measure the
//! fixture rather than the parser. What remains inside the measured region is
//! reading the records, copying their fields, and walking them.
//!
//! # Fairness
//!
//! All four cases read the same `static` corpus with the same dialect, headers
//! disabled, an 8 KiB buffer where a buffer exists, and return the same
//! checksum, which is asserted rather than assumed. The two slice-fed cases
//! (`slice`, `push`) are handed bytes that are already in memory, so they do no
//! I/O at all; `io` and `csv` both read through a `Cursor` over the same bytes.
//! A `Cursor` read is close to free, so this isolates the buffering machinery
//! rather than any real device.
//!
//! The counts here are Callgrind instructions, which cannot see memory traffic.
//! Copying a field into an owned record is exactly the kind of work that costs
//! bandwidth rather than instructions, so read this table as a comparison
//! between cases doing the same copying, not as a measure of what copying costs.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, IoParser, PushParser, SliceParser};
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
fn record() -> ByteRecord {
    ByteRecord::with_capacity(FIELDS, ROW_LEN)
}

// ── setup: everything that is not per-record work ────────────────────────────

type SliceState = (SliceParser<'static, Csv>, ByteRecord, &'static [u8]);
type IoState = (
    IoParser<Cursor<&'static [u8]>, Csv>,
    ByteRecord,
    &'static [u8],
);
type PushState = (PushParser<Csv>, ByteRecord, &'static [u8]);
type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    &'static [u8],
);

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
        ::csv::ByteRecord::with_capacity(ROW_LEN, FIELDS),
        input,
    )
}

// ── the measured bodies ──────────────────────────────────────────────────────

fn sum(record: &ByteRecord) -> u64 {
    let mut total = 0_u64;
    for index in 0..record.len() {
        total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
    }
    total
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = slice_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = slice_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = slice_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = slice_state, teardown = drop_it)]
fn slice(state: SliceState) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
    let (mut parser, mut record, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
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
fn io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord) {
    let (mut parser, mut record, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
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
fn push(state: PushState) -> (u64, PushParser<Csv>, ByteRecord) {
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

fn drain(chunk: &mut Chunk<'_, '_, Csv>, record: &mut ByteRecord) -> u64 {
    let mut total = 0_u64;
    while let Some(mut line) = chunk
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(record));
    }
    total
}

// The `csv` crate reading the same bytes into its own reusable `ByteRecord`.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = csv_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = csv_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = csv_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = csv_state, teardown = drop_it)]
fn csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>, ::csv::ByteRecord) {
    let (mut reader, mut record, input) = state;
    let mut total = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, input)), reader, record)
}

library_benchmark_group!(
    name = byte_record;
    benchmarks = slice, io, push, csv
);

main!(library_benchmark_groups = byte_record);
