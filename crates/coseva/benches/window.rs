//! What a record costs when it does not fit in the window.
//!
//! Every other suite here measures `io` and `push` with an 8 KiB buffer against
//! 51-byte records, so a record crosses a refill roughly once in 160 and the
//! paths that handle it — shifting the window, copying a partial record into
//! owned scratch, reclaiming that scratch afterwards — are effectively
//! unmeasured. This suite drives the buffer down through the record size until
//! every record straddles, so those paths are all that is left.
//!
//! # The two axes
//!
//! `io` sweeps `buffer_capacity`, which is what a caller actually sets. `push`
//! sweeps the size of the slices handed to `chunk`, which is the equivalent
//! knob for a caller who is not choosing it at all — it is whatever their
//! transport delivers. A push caller cannot set the window; they can only be
//! given small pieces, and this measures what that costs them.
//!
//! Both bottom out below the 51-byte record, where no record can ever be
//! completed from one window's worth of input.
//!
//! # Results
//!
//! Callgrind instruction counts for 1000 records into a `ByteRecord`. Per
//! record is the count divided by 1000, and `vs 8192` is the cost relative to
//! the default window.
//!
//! | Bytes | `io`      | per record | vs 8192 | `push`    | per record | vs 8192 |
//! |-------|-----------|------------|---------|-----------|------------|---------|
//! | 8192  | 1,039,508 |      1,040 |         |   984,448 |        984 |         |
//! | 1024  | 1,093,951 |      1,094 |   +5.2% | 1,127,216 |      1,127 |  +14.5% |
//! | 256   | 1,329,885 |      1,330 |  +27.9% | 1,613,418 |      1,613 |  +63.9% |
//! | 128   | 1,507,748 |      1,508 |  +45.0% | 2,259,666 |      2,260 | +129.5% |
//! | 64    | 2,402,637 |      2,403 | +131.1% | 2,814,152 |      2,814 | +185.9% |
//! | 32    | 2,404,125 |      2,404 | +131.3% | 3,219,957 |      3,220 | +227.1% |
//!
//! # What the numbers say
//!
//! `buffer_capacity` buys little above about 1 KiB. Dropping the window from
//! 8 KiB to 1 KiB costs `io` 5.2%, which is a rounding error against the
//! eightfold memory saving, and a caller with many concurrent parsers should
//! know that. Below that it starts to matter: 28% at 256 bytes, 45% at 128, and
//! 131% at 64, where five records share a window and every one of them
//! straddles.
//!
//! `io` then stops degrading. 32 bytes costs 2,404,125 against 64 bytes'
//! 2,402,637 — 0.06% apart, when the window has halved and is now smaller than
//! a single record. That is the floor: below the record size the parser is
//! growing owned scratch to hold the record rather than shifting a window, and
//! the cost does not depend on how small the window was. A caller cannot make
//! `io` worse than about 2.3× its best by choosing a bad capacity.
//!
//! `push` has no such floor for a record swept this short. It is marginally
//! *cheaper* than `io` at the default — 984 against 1,040, since it is handed
//! its bytes rather than reading them — and dearer as the chunk shrinks, 3,220
//! against 2,404 at 32 bytes. A push caller does not choose the chunk size: it
//! is whatever their transport hands them, and a transport delivering 32-byte
//! writes makes their parse costlier. `PushParser`'s documentation now says so,
//! with these numbers, so a caller can coalesce before lending.
//!
//! # Resuming a record instead of reparsing it
//!
//! The 51-byte record above straddles a small window at most once — one chunk
//! holds a prefix, the next completes it — so the record is copied into scratch
//! and parsed once it can finish. That single parse is cheap, which is why the
//! columns above are dominated by per-read and per-chunk overhead rather than
//! by re-parsing.
//!
//! A record longer than the window is a different matter. No window below its
//! own length can hold it, so the parser grows owned scratch and absorbs the
//! record a chunk at a time. Parsing the
//! whole accumulated prefix from the record's start on each arriving chunk
//! would be quadratic in the
//! record length and ruinous at 32 bytes, where a four-kilobyte record is grown
//! in more than a hundred steps.
//!
//! The engine carries a resume checkpoint for exactly this case. When a
//! full parse leaves a record incomplete — and only then — it remembers how far
//! it scanned and the field and quoting state it was in. The next, wider window
//! resumes the boundary scan from there rather than from the record's start,
//! proves in one pass over the newly arrived bytes that the record still cannot
//! complete, and skips the doomed full parse. Every byte of the record is
//! scanned once no matter how many chunks deliver it, so absorption is linear;
//! the final chunk, the one that lets the record finish, runs the full parse
//! once and materializes it. The fast path is untouched: a checkpoint exists
//! only for a record a narrower window already refused, so a record that fits
//! the window it first appears in never reaches the scan and pays a single
//! integer comparison per record.
//!
//! The scan reproduces the record-boundary rules of every dialect: quote
//! syntax on or off, doubled-quote and backslash and `MySQL` and unquoted
//! escapes, CRLF endings, skip-initial-space, and the Compatible recovery
//! permits. Comment and blank skipping has already positioned the checkpoint
//! at the data record. Multi-byte separator leads are confirmed against their
//! tails, and a tail split by a refill pauses at the lead byte. On the push
//! front end a second guard absorbs a terminator-free chunk without handing
//! the window to the engine at all.
//!
//! # A record longer than any window
//!
//! The sweep above holds the record at 51 bytes, so it exercises the resumable
//! path at most once per record. This corpus makes every record 4000 bytes of a
//! single unquoted field, so a window below the record size can never hold it
//! and the parser grows owned scratch instead. Eight such records are swept at
//! three window sizes.
//!
//! Callgrind instruction counts for the eight-record corpus; per record is the
//! count divided by 8.
//!
//! | Bytes | `io_long` | per record | vs 8192 | `push_long` | per record | vs 8192 |
//! |-------|-----------|------------|---------|-------------|------------|---------|
//! | 8192  |    94,712 |     11,839 |         |      95,384 |     11,923 |         |
//! | 256   |   106,081 |     13,260 |  +12.0% |     145,824 |     18,228 |  +52.9% |
//! | 32    |   170,868 |     21,359 |  +80.4% |     429,285 |     53,661 | +350.1% |
//!
//! The shape is linear, which is the point. `io_long` at 32 bytes is 1.80× its
//! 8 KiB cost, and `push_long` 4.50× — where before resume a record grown in
//! 125 steps cost quadratically more, `push_long` alone having been 7.0× at 32
//! bytes. The clearest evidence is per byte: a 32-byte `push_long` chunk absorbs
//! its corpus at 13.4 instructions per byte against the fixed 51-byte `push`
//! sweep's 63 at the same chunk size. A record eighty times longer is nearly
//! five times *cheaper* per byte, which a quadratic reparse could never be.
//! Within 50% of the 8 KiB cost is reached for `io` down to a few hundred bytes;
//! `push` reaches it at 256 bytes and no lower.
//!
//! # The irreducible per-chunk floor
//!
//! `push_long` at 32 bytes stays 4.50× its 8 KiB cost, well outside 50%, and no
//! change to the parser closes that gap, because it is not parsing. `push_floor`
//! measures why: it feeds the same total bytes as one field that never
//! completes a record, so every chunk is absorbed and refused and the engine
//! materializes exactly one record at the very end. What is left is the pure
//! per-chunk lifecycle — construct the loan, scan the chunk for a terminator,
//! settle, and `done` — and nothing else.
//!
//! | Bytes | `push_long` | `push_floor` |
//! |-------|-------------|--------------|
//! | 8192  |      95,384 |      124,341 |
//! | 256   |     145,824 |      152,223 |
//! | 32    |     429,285 |      435,656 |
//!
//! Read it as a slope, not a level: going from 256-byte to 32-byte chunks —
//! from 126 loans to 1001 for this corpus — costs `push_long` 283,461 more
//! instructions and `push_floor` 283,433, the same 324 instructions per chunk to
//! within a rounding error. The marginal cost of a smaller chunk is *entirely*
//! the per-chunk round trip; the resumable scan has driven the reparse component
//! of it to zero. An 8 KiB window pays that round trip 256 times less often for
//! the same bytes, so no parser can bring 32-byte push chunks within 50% of it —
//! the ~324 instructions each of a thousand loans costs is a floor a caller
//! escapes only by handing over larger pieces. `push_floor` sits a little above
//! `push_long` because its window grows monotonically instead of resetting per
//! record, which only makes it a conservative bound on that floor.
//!
//! # Comment/blank-skip and multi-byte formats
//!
//! The two option families whose boundary machinery complicates resume are
//! swept separately over
//! the same long corpus. `skipping` enables both a comment marker and skipped
//! blank records; `multibyte` uses `||` as its delimiter. The data does not
//! contain either marker, which isolates the option's boundary machinery from
//! the cost of discarding extra physical lines.
//!
//! | Case | 8192 | 256 | 32 |
//! |---|---:|---:|---:|
//! | `io_long_skipping` | 91,068 | 113,030 | 201,095 |
//! | `push_long_skipping` | 76,759 | 131,651 | 415,224 |
//! | `io_long_multibyte` | 68,350 | 84,471 | 171,119 |
//! | `push_long_multibyte` | 68,290 | 124,518 | 408,283 |
//!
//! Resume keeps both I/O rows off the quadratic curve at 32 bytes. Push does
//! not depend on it: its terminator-free absorption
//! avoids prefix reparsing on its own. From 8 KiB to 32 bytes the two push rows add
//! 338,465 and 339,993 instructions; the no-op loan floor adds about 311,316.
//! The parser-specific remainder is therefore 27,149 (35.4% of the skipping
//! 8-KiB cost) and 28,677 (42.0% of the multi-byte 8-KiB cost), both inside the
//! 50% target after subtracting the irreducible per-loan slope.
//!
//! Long quoted, CRLF, escaped, skip-initial-space, and recovery fields are
//! covered at one-byte boundaries by the test suite rather than repeated here.
//!
//! Numbers in this file are comparable only to each other; `fixture.rs`
//! records the measurement showing why.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{BlankRecords, FormatOptions, Headers, ParseOptions};
use coseva::format::{Csv, CsvFormat, Dynamic};
use coseva::{ByteRecord, Chunk, IoParser, PushParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this suite sweeps window size rather than record count, so it uses one corpus"
)]
mod fixture;

use fixture::{FIELDS, ROW_LEN, ROWS_1000, check, drop_it};

/// The window sizes swept, from the default down past the 51-byte record.
const DEFAULT: usize = 8192;
const SMALL: usize = 1024;
const TIGHT: usize = 256;
const HALF: usize = 128;
const BELOW: usize = 64;
const SPLIT: usize = 32;

fn sum(record: &ByteRecord) -> u64 {
    let mut total = 0_u64;
    for index in 0..record.len() {
        total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
    }
    total
}

fn record() -> ByteRecord {
    ByteRecord::with_capacity(FIELDS, ROW_LEN)
}

// ── io: the caller sets the window ───────────────────────────────────────────

type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord);

fn io_state(capacity: usize) -> IoState {
    let options = ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(capacity);
    let parser = IoParser::<_, Csv>::new(Cursor::new(ROWS_1000), options)
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record())
}

#[library_benchmark]
#[bench::bytes_8192(args = (DEFAULT), setup = io_state, teardown = drop_it)]
#[bench::bytes_1024(args = (SMALL), setup = io_state, teardown = drop_it)]
#[bench::bytes_256(args = (TIGHT), setup = io_state, teardown = drop_it)]
#[bench::bytes_128(args = (HALF), setup = io_state, teardown = drop_it)]
#[bench::bytes_64(args = (BELOW), setup = io_state, teardown = drop_it)]
#[bench::bytes_32(args = (SPLIT), setup = io_state, teardown = drop_it)]
fn io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord) {
    let (mut parser, mut record) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, ROWS_1000)), parser, record)
}

// ── push: the transport sets the window ──────────────────────────────────────

type PushState = (PushParser<Csv>, ByteRecord, usize);

fn push_state(step: usize) -> PushState {
    let options = ParseOptions::new().headers(Headers::None);
    let parser = PushParser::<Csv>::new(options)
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), step)
}

#[library_benchmark]
#[bench::bytes_8192(args = (DEFAULT), setup = push_state, teardown = drop_it)]
#[bench::bytes_1024(args = (SMALL), setup = push_state, teardown = drop_it)]
#[bench::bytes_256(args = (TIGHT), setup = push_state, teardown = drop_it)]
#[bench::bytes_128(args = (HALF), setup = push_state, teardown = drop_it)]
#[bench::bytes_64(args = (BELOW), setup = push_state, teardown = drop_it)]
#[bench::bytes_32(args = (SPLIT), setup = push_state, teardown = drop_it)]
fn push(state: PushState) -> (u64, PushParser<Csv>, ByteRecord) {
    let (mut parser, mut record, step) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < ROWS_1000.len() {
        let end = (fed + step).min(ROWS_1000.len());
        let mut chunk = parser.chunk(&ROWS_1000[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(check(total, ROWS_1000)), parser, record)
}

fn drain<F: CsvFormat>(chunk: &mut Chunk<'_, '_, F>, record: &mut ByteRecord) -> u64 {
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

// ── A record longer than any window: the resume-checkpoint shape ─────────────
//
// The sweep above holds the record at 51 bytes, so a straddle copies one short
// record into scratch and reparses it whole. That reparse is cheap and it
// happens once per record, which is why resume barely moves those columns.
//
// This corpus makes every record kilobytes long, so a window below the record
// size can never hold it and the parser grows owned scratch instead, absorbing
// the record a chunk at a time. Before resumable scans, each growth reparsed the record from
// its start — quadratic in the record length, and ruinous at 32 bytes where a
// four-kilobyte record is absorbed in more than a hundred steps. Resume scans
// each byte once no matter how it arrives, so the same record costs the same
// whether it lands whole or one chunk at a time, and 32-byte chunks fall within
// reach of the 8 KiB window rather than orders of magnitude beyond it.

/// The length of the single field in each long record.
const LONG_FIELD: usize = 4000;

/// The number of long records in the corpus.
const LONG_ROWS: usize = 8;

/// The width of one long record: its field plus a terminating newline.
const LONG_LEN: usize = LONG_FIELD + 1;

/// The lowercase alphabet, cycled to fill a corpus field with plain text that
/// carries no delimiter, quote, escape, or terminator.
const ALPHABET: [u8; 26] = *b"abcdefghijklmnopqrstuvwxyz";

/// Build a corpus of [`LONG_ROWS`] single-field records at compile time, each
/// [`LONG_FIELD`] bytes of plain text with no delimiter, quote, or newline
/// inside it, terminated by a newline.
const fn long_corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        let column = index % LONG_LEN;
        out[index] = if column == LONG_FIELD {
            b'\n'
        } else {
            ALPHABET[column % 26]
        };
        index += 1;
    }
    out
}

static LONG_BUF: [u8; LONG_LEN * LONG_ROWS] = long_corpus();

/// The long-record corpus: [`LONG_ROWS`] records of [`LONG_FIELD`] bytes each.
static LONG: &[u8] = &LONG_BUF;

/// Assert the case walked every long record's single field exactly once.
fn long_check(total: u64) -> u64 {
    let expected = (LONG_ROWS * LONG_FIELD) as u64;
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    total
}

type IoLongState = (IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord);

fn io_long_state(capacity: usize) -> IoLongState {
    let options = ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(capacity);
    let parser = IoParser::<_, Csv>::new(Cursor::new(LONG), options)
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(1, LONG_LEN))
}

#[library_benchmark]
#[bench::bytes_8192(args = (DEFAULT), setup = io_long_state, teardown = drop_it)]
#[bench::bytes_256(args = (TIGHT), setup = io_long_state, teardown = drop_it)]
#[bench::bytes_32(args = (SPLIT), setup = io_long_state, teardown = drop_it)]
fn io_long(state: IoLongState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord) {
    let (mut parser, mut record) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(long_check(total)), parser, record)
}

type PushLongState = (PushParser<Csv>, ByteRecord, usize);

fn push_long_state(step: usize) -> PushLongState {
    let options = ParseOptions::new().headers(Headers::None);
    let parser = PushParser::<Csv>::new(options)
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(1, LONG_LEN), step)
}

#[library_benchmark]
#[bench::bytes_8192(args = (DEFAULT), setup = push_long_state, teardown = drop_it)]
#[bench::bytes_256(args = (TIGHT), setup = push_long_state, teardown = drop_it)]
#[bench::bytes_32(args = (SPLIT), setup = push_long_state, teardown = drop_it)]
fn push_long(state: PushLongState) -> (u64, PushParser<Csv>, ByteRecord) {
    let (mut parser, mut record, step) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < LONG.len() {
        let end = (fed + step).min(LONG.len());
        let mut chunk = parser.chunk(&LONG[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(long_check(total)), parser, record)
}

// ── The irreducible per-chunk floor: a record that never completes ───────────
//
// `push_long` finishes a record every `LONG_LEN` bytes, so its columns mix the
// per-chunk lifecycle with eight full parses and their owned copies. This
// corpus is one field the width of the whole corpus with no terminator inside
// it, so no record ever completes until the stream closes: every chunk is
// absorbed and refused, and the engine materializes exactly one record at the
// very end. What is left in the swept columns is the pure chunk-loan floor --
// constructing the loan, scanning the chunk for a terminator, the settle, and
// `done` -- paid once per chunk and nothing else. Subtracting this from
// `push_long` at the same chunk size isolates how much of a long record's cost
// is the per-chunk round trip a caller cannot avoid by any change to the
// parser, and how much is parsing the record. The window grows monotonically
// here rather than resetting per record, so the floor slightly overstates the
// absorb cost, which only strengthens it as a lower bound on the round trip.

/// A single field the width of the whole long corpus, carrying no delimiter,
/// quote, or terminator, so the stream is one record that never completes
/// until it is finished.
const fn floor_corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        out[index] = ALPHABET[index % 26];
        index += 1;
    }
    out
}

static FLOOR_BUF: [u8; LONG_LEN * LONG_ROWS] = floor_corpus();

/// The floor corpus: one never-completing field the size of [`LONG`].
static FLOOR: &[u8] = &FLOOR_BUF;

/// Assert the case walked the whole corpus into a single field exactly once.
fn floor_check(total: u64) -> u64 {
    let expected = FLOOR.len() as u64;
    assert_eq!(total, expected, "floor benchmark parsed the wrong bytes");
    total
}

type PushFloorState = (PushParser<Csv>, ByteRecord, usize);

fn push_floor_state(step: usize) -> PushFloorState {
    let options = ParseOptions::new().headers(Headers::None);
    let parser = PushParser::<Csv>::new(options)
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(1, FLOOR.len()), step)
}

#[library_benchmark]
#[bench::bytes_8192(args = (DEFAULT), setup = push_floor_state, teardown = drop_it)]
#[bench::bytes_256(args = (TIGHT), setup = push_floor_state, teardown = drop_it)]
#[bench::bytes_32(args = (SPLIT), setup = push_floor_state, teardown = drop_it)]
fn push_floor(state: PushFloorState) -> (u64, PushParser<Csv>, ByteRecord) {
    let (mut parser, mut record, step) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < FLOOR.len() {
        let end = (fed + step).min(FLOOR.len());
        let mut chunk = parser.chunk(&FLOOR[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(floor_check(total)), parser, record)
}

// ── Dialects that deliberately decline the resume checkpoint ────────────────

type IoDeclinedState = (IoParser<Cursor<&'static [u8]>, Dynamic>, ByteRecord, usize);

fn io_declined_state(input: (&'static [u8], FormatOptions, usize)) -> IoDeclinedState {
    let (bytes, format, capacity) = input;
    let parser = IoParser::<_, Dynamic>::with_options(
        Cursor::new(bytes),
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(capacity),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(1, LONG_LEN), LONG_ROWS)
}

fn run_io_declined(
    state: IoDeclinedState,
) -> (u64, IoParser<Cursor<&'static [u8]>, Dynamic>, ByteRecord) {
    let (mut parser, mut record, rows) = state;
    let mut total = 0_u64;
    let mut seen = 0_usize;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
        seen += 1;
    }
    assert_eq!(seen, rows, "benchmark parsed the wrong record count");
    (black_box(long_check(total)), parser, record)
}

type PushDeclinedState = (PushParser<Dynamic>, ByteRecord, usize);

fn push_declined_state(input: (FormatOptions, usize)) -> PushDeclinedState {
    let (format, step) = input;
    let parser =
        PushParser::<Dynamic>::with_options(format, ParseOptions::new().headers(Headers::None))
            .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(1, LONG_LEN), step)
}

fn run_push_declined(state: PushDeclinedState) -> (u64, PushParser<Dynamic>, ByteRecord) {
    let (mut parser, mut record, step) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < LONG.len() {
        let end = (fed + step).min(LONG.len());
        let mut chunk = parser.chunk(&LONG[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(long_check(total)), parser, record)
}

const SKIPPING: FormatOptions = FormatOptions::CSV
    .comment(Some(b'#'))
    .blank_records(BlankRecords::Skip);
const MULTIBYTE: FormatOptions = FormatOptions::CSV.delimiter_sequence(b"||");

#[library_benchmark]
#[bench::bytes_8192(
        args = ((LONG, SKIPPING, DEFAULT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_256(
        args = ((LONG, SKIPPING, TIGHT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_32(
        args = ((LONG, SKIPPING, SPLIT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
fn io_long_skipping(
    state: IoDeclinedState,
) -> (u64, IoParser<Cursor<&'static [u8]>, Dynamic>, ByteRecord) {
    run_io_declined(state)
}

#[library_benchmark]
#[bench::bytes_8192(
        args = ((SKIPPING, DEFAULT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_256(
        args = ((SKIPPING, TIGHT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_32(
        args = ((SKIPPING, SPLIT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
fn push_long_skipping(state: PushDeclinedState) -> (u64, PushParser<Dynamic>, ByteRecord) {
    run_push_declined(state)
}

#[library_benchmark]
#[bench::bytes_8192(
        args = ((LONG, MULTIBYTE, DEFAULT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_256(
        args = ((LONG, MULTIBYTE, TIGHT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_32(
        args = ((LONG, MULTIBYTE, SPLIT)),
        setup = io_declined_state,
        teardown = drop_it
    )]
fn io_long_multibyte(
    state: IoDeclinedState,
) -> (u64, IoParser<Cursor<&'static [u8]>, Dynamic>, ByteRecord) {
    run_io_declined(state)
}

#[library_benchmark]
#[bench::bytes_8192(
        args = ((MULTIBYTE, DEFAULT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_256(
        args = ((MULTIBYTE, TIGHT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
#[bench::bytes_32(
        args = ((MULTIBYTE, SPLIT)),
        setup = push_declined_state,
        teardown = drop_it
    )]
fn push_long_multibyte(state: PushDeclinedState) -> (u64, PushParser<Dynamic>, ByteRecord) {
    run_push_declined(state)
}

library_benchmark_group!(
    name = window;
    benchmarks =
        io,
        push,
        io_long,
        push_long,
        push_floor,
        io_long_skipping,
        push_long_skipping,
        io_long_multibyte,
        push_long_multibyte
);

main!(library_benchmark_groups = window);
