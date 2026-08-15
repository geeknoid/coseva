//! What it costs to write a record, across the three emitters and against the
//! `csv` writer.
//!
//! Every other suite in this directory reads. Writing had no numbers at all,
//! which meant half the crate — `emit.rs`, three emitters and the `CsvEncode`
//! derive — could regress without anything noticing.
//!
//! # The corpus
//!
//! The same six fields the read suites parse, taken apart into the slices that
//! produced them, so a row written here is byte-for-byte the row read there.
//! Each case asserts the total bytes it produced, which pins the output to
//! exactly the corpus the read suites consume and stops a case drifting into
//! writing something cheaper.
//!
//! Row counts are 1, 10, 100 and 1000, as elsewhere. A single size conflates
//! what an emitter costs once with what it costs per record; only the
//! difference between sizes is a writing speed.
//!
//! # The cases
//!
//! `vec`, `io` and `push` write six borrowed slices through each of the three
//! emitters, against `csv_writer` doing the same through `csv::Writer`. The
//! three sinks are all in memory, so no case is measuring the operating
//! system.
//!
//! `vec_encode` writes the same row from a struct through `#[derive(CsvEncode)]`
//! and `vec_serialize` writes it through Serde, against `csv_serialize`. Read
//! against each other these say what native encoding saves, which is the same
//! question `decode` and `deserialize` ask of the read path.
//!
//! # Results
//!
//! Callgrind instruction counts. "Per record" is the marginal cost, taken from
//! the difference between 100 and 1000 rows so that whatever an emitter does
//! once falls out of it.
//!
//! Writing borrowed slices:
//!
//! | Case         | 1     | 10     | 100     | 1000      | Per record | vs `csv` |
//! |--------------|-------|--------|---------|-----------|------------|----------|
//! | `vec`        |   979 |  8,836 |  87,406 |   873,106 |        873 | -26%     |
//! | `push`       | 1,192 | 10,597 | 104,647 |   902,937 |        887 | -25%     |
//! | `io`         |   980 |  5,068 |  45,730 |   498,477 |        503 | -58%     |
//! | `csv_writer` | 1,555 | 12,222 | 118,674 | 1,183,917 |      1,184 |          |
//!
//! Writing a struct:
//!
//! | Case             | 1     | 10     | 100     | 1000      | Per record | vs `csv` |
//! |------------------|-------|--------|---------|-----------|------------|----------|
//! | `vec_encode`     | 1,942 | 18,592 | 185,092 | 1,850,092 |      1,850 | -13%     |
//! | `vec_serialize`  | 2,139 | 20,553 | 204,693 | 2,046,093 |      2,046 | -3%      |
//! | `csv_serialize`  | 2,484 | 21,539 | 211,871 | 2,115,906 |      2,116 |          |
//!
//! # Writing slices is where the advantage is
//!
//! The direct vector and push paths cost 873 and 887 instructions per record.
//! Against the `csv` writer's 1,184, they save 26% and 25%. The buffered I/O
//! path is lower at 503 over the final range because that range amortises its
//! internal buffer flushes differently; it is not a direct-vector baseline.
//!
//! `vec` is exactly linear: 873.0 instructions per record between 10 and 100
//! rows and 873.0 between 100 and 1000. `io` is not — about 452 per record over
//! the middle range and 503 over the final one — and flushing its 8 KiB buffer into
//! the sink is the only work it does that scales differently from the others.
//! This suite does not isolate that, and the honest statement is that `io`'s
//! marginal cost rises with the number of flushes rather than that it is 503.
//!
//! # Writing a struct is a different question, and mostly not a CSV one
//!
//! The typed cases cost more than the slice cases, but retain measurable
//! advantages over `csv`: 12.6% for the derive and 3.3% for Serde.
//!
//! That is not the encoder losing its edge. The slice cases copy text that is
//! already text. The typed cases render two `f64` and a `u32` from their binary
//! forms first, and float formatting is expensive on any path. Both sides pay
//! it — `csv_serialize` is 2,116 against `csv_writer`'s 1,184, a 1.8-fold rise
//! against coseva's roughly twofold rise — so the comparison stays fair while the thing
//! being compared becomes substantially `core::fmt` rather than CSV.
//!
//! All three typed cases are effectively linear — 1,850.0, 2,046.0 and 2,114.8
//! instructions per record between 10 and 100 rows, and 1,850.0, 2,046.0 and
//! 2,115.6 between 100 and 1000 — so the per-record figures are trustworthy
//! even though what they are made of is mixed.
//!
//! This suite does not separate the two, and the numbers above should not be
//! read as though it did. A corpus of string fields would say what typed
//! encoding costs without formatting, and it is the obvious next measurement.
//!
//! # The derive saves much less here than it does on the read path
//!
//! `vec_encode` beats `vec_serialize` by 9.6%. On the read path the same
//! comparison is worth far more: `decode`'s slice case is 657 instructions per
//! record against `deserialize`'s 957, a 31% saving.
//!
//! The likely explanation — reasoned from the two paths, not measured here —
//! is what Serde has to do in each direction. Reading through Serde drives a
//! `Deserializer` that visits fields by name and works out where each one is,
//! while the derive resolves that mapping once and indexes directly. Writing
//! has no such lookup on either side, because fields come out in declaration
//! order regardless, so the derive's advantage is confined to skipping the
//! `Serializer` dispatch, and the shared formatting cost dilutes even that.
//! Confirming this would need the string-field corpus described above.
//!
//! The practical reading is that `#[derive(CsvEncode)]` is worth choosing for
//! its API rather than for its speed, and that anyone hoping to make struct
//! writing faster should look at how fields are rendered before looking at how
//! they are dispatched.
//!
//! # What this does not measure
//!
//! The first `encode` group remains an unquoted baseline. The `activated`
//! group below isolates every quoting and escaping policy on fields that make
//! it fire, and compares direct, native-derived and Serde emission over
//! byte-identical asserted output.
//!
//! Writing to a file. `io` writes to a `Vec<u8>`, so its numbers are the
//! emitter's own cost and not the cost of a syscall.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Write;

use coseva::config::{EmitOptions, Escape, FormatOptions, Quoting};
use coseva::encoding::CsvEncode;
use coseva::format::{Csv, Dynamic};
use coseva::{IoEmitter, PushEmitter, VecEmitter, encode_to_writer};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this file writes the corpus rather than reading it"
)]
mod fixture;

use fixture::{BUFFER, ROW, ROW_LEN, drop_it};

/// The six fields of the read suites' row, as the slices that built it.
static FIELDS: [&[u8]; 6] = [
    b"Boston",
    b"Massachusetts",
    b"4500000",
    b"42.3601",
    b"-71.0589",
    b"true",
];

/// The same row as a struct, for the derive and Serde cases.
#[derive(Clone, CsvEncode, ::serde::Serialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the field names are the corpus's header names"
)]
struct City {
    city: &'static str,
    state: &'static str,
    population: u32,
    latitude: f64,
    longitude: f64,
    coastal: bool,
}

static CITY: City = City {
    city: "Boston",
    state: "Massachusetts",
    population: 4_500_000,
    latitude: 42.3601,
    longitude: -71.0589,
    coastal: true,
};

/// A narrow row of two string fields, for the staging measurement.
///
/// String fields keep the case measuring what typed emission itself costs —
/// framing a field and, before direct typed emission, staging it in a
/// [`ByteRecord`] — rather
/// than the `core::fmt` rendering of a float, which every path pays equally.
static NARROW_FIELDS: [&[u8]; 2] = [b"Boston", b"Massachusetts"];

/// A wide row of twelve string fields, for the same measurement at width.
///
/// Staging copies every field once, so its cost grows with the field count;
/// a wide row is where an intermediate copy would show up most.
static WIDE_FIELDS: [&[u8]; 12] = [
    b"Boston",
    b"Massachusetts",
    b"Suffolk",
    b"United States",
    b"North America",
    b"Eastern",
    b"coastal",
    b"harbor",
    b"colonial",
    b"seaport",
    b"historic",
    b"walkable",
];

/// The narrow row as a struct, for the derive and Serde cases.
#[derive(Clone, CsvEncode, ::serde::Serialize)]
struct Narrow {
    city: &'static str,
    state: &'static str,
}

static NARROW: Narrow = Narrow {
    city: "Boston",
    state: "Massachusetts",
};

/// The wide row as a struct, its fields in the same order as [`WIDE_FIELDS`].
#[derive(Clone, CsvEncode, ::serde::Serialize)]
struct Wide {
    city: &'static str,
    state: &'static str,
    county: &'static str,
    country: &'static str,
    continent: &'static str,
    timezone: &'static str,
    terrain: &'static str,
    feature: &'static str,
    era: &'static str,
    kind: &'static str,
    note: &'static str,
    trait_: &'static str,
}

static WIDE: Wide = Wide {
    city: "Boston",
    state: "Massachusetts",
    county: "Suffolk",
    country: "United States",
    continent: "North America",
    timezone: "Eastern",
    terrain: "coastal",
    feature: "harbor",
    era: "colonial",
    kind: "seaport",
    note: "historic",
    trait_: "walkable",
};

/// String-only typed row whose contents force both quoting and quote escaping.
#[derive(Clone, CsvEncode, ::serde::Serialize)]
struct Activated {
    left: &'static str,
    right: &'static str,
}

static ACTIVATED: Activated = Activated {
    left: "a,b",
    right: "say \"hi\"",
};

/// The exact bytes a row of `fields` occupies once framed: every field, a
/// delimiter between each pair, and a terminating newline.
fn corpus_len(fields: &[&[u8]]) -> usize {
    fields.iter().map(|field| field.len()).sum::<usize>() + fields.len()
}

/// Write `rows` copies of `fields` through the field-at-a-time builder, and
/// return the byte count so the caller can black-box it.
fn build_rows(emitter: &mut VecEmitter<Csv>, rows: usize, fields: &[&[u8]]) -> usize {
    for _ in 0..rows {
        let mut pending = emitter.begin_record();
        for field in fields {
            pending
                .write_field(field)
                .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
        }
        pending
            .finish()
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    rows * corpus_len(fields)
}

fn options() -> EmitOptions {
    EmitOptions::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
}

/// Assert the case wrote exactly the corpus the read suites parse.
///
/// Asserted rather than assumed, so a case cannot quietly drift into writing
/// something shorter and still look comparable.
fn check(written: usize, rows: usize) -> usize {
    assert_eq!(written, rows * ROW_LEN, "benchmark wrote the wrong bytes");
    written
}

/// Assert the derive and Serde cases wrote as many rows as they were asked to.
///
/// Their rows are not byte-identical to the corpus — a `f64` renders through
/// its own formatting, not the corpus text — so these check the row count
/// rather than the exact width.
///
/// Run as a teardown rather than inside the body, because counting newlines is
/// a scan over the whole output and measuring it would add work to the typed
/// cases that the slice cases, whose check is a length comparison, never pay.
#[expect(
    clippy::naive_bytecount,
    reason = "this runs outside the measured region, where a dependency would not earn itself"
)]
fn check_rows((output, rows): (Vec<u8>, usize)) {
    if output.len() == rows * ROW_LEN {
        for row in output.chunks_exact(ROW_LEN) {
            assert_eq!(row, ROW, "benchmark wrote different CSV bytes");
        }
        return;
    }
    let count = output.iter().filter(|&&byte| byte == b'\n').count();
    assert_eq!(count, rows, "benchmark wrote the wrong number of rows");
}

// ── setup: everything that is not per-record work ────────────────────────────

/// A sink large enough for the widest corpus, so no case grows one while being
/// measured.
fn sink() -> Vec<u8> {
    Vec::with_capacity(ROW_LEN * 1000 + 1024)
}

/// A sink sized for the [`WIDE_FIELDS`] corpus, whose rows are far wider than
/// the read suites' row. Without it a 1000-row wide case would outgrow [`sink`]
/// and reallocate inside the measured region, and the marginal cost would stop
/// being linear.
fn wide_sink() -> Vec<u8> {
    Vec::with_capacity(corpus_len(&WIDE_FIELDS) * 1000 + 1024)
}

type VecState = (VecEmitter<Csv>, usize);
type IoState = (IoEmitter<Vec<u8>, Csv>, usize);
type PushState = (PushEmitter<Csv>, usize);
type CsvState = (::csv::Writer<Vec<u8>>, usize);

fn vec_state(rows: usize) -> VecState {
    let emitter = VecEmitter::<Csv>::new(sink(), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, rows)
}

/// [`vec_state`] with a sink sized for the wide corpus.
fn wide_vec_state(rows: usize) -> VecState {
    let emitter = VecEmitter::<Csv>::new(wide_sink(), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, rows)
}

fn io_state(rows: usize) -> IoState {
    let emitter = IoEmitter::<_, Csv>::new(sink(), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, rows)
}

fn push_state(rows: usize) -> PushState {
    let emitter = PushEmitter::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, rows)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn csv_state(rows: usize) -> CsvState {
    let writer = ::csv::WriterBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_writer(sink());
    (writer, rows)
}

// ── the measured bodies ──────────────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = check_rows)]
fn vec(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .emit_slices(&FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = emitter.into_inner();
    check(output.len(), rows);
    (black_box(output), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = io_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = io_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = io_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = io_state, teardown = check_rows)]
fn io(state: IoState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .emit_record(FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = emitter
        .into_inner()
        .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    check(output.len(), rows);
    (black_box(output), rows)
}

// `push` hands its buffer back rather than owning a sink, so this drains it
// once the buffer has filled — the pattern the front end exists for. The drain
// is inside the measured region because it is part of what the caller pays.
#[library_benchmark]
#[bench::rows_1(args = (1), setup = push_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = push_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = push_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = push_state, teardown = drop_it)]
fn push(state: PushState) -> (usize, PushEmitter<Csv>) {
    let (mut emitter, rows) = state;
    let mut written = 0_usize;
    for _ in 0..rows {
        emitter
            .emit_slices(&FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
        if emitter.buffer().len() >= BUFFER {
            written += emitter.buffer().len();
            emitter.clear();
        }
    }
    written += emitter.buffer().len();
    (black_box(check(written, rows)), emitter)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = csv_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = csv_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = csv_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = csv_state, teardown = check_rows)]
fn csv_writer(state: CsvState) -> (Vec<u8>, usize) {
    let (mut writer, rows) = state;
    for _ in 0..rows {
        writer
            .write_record(FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = writer
        .into_inner()
        .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    check(output.len(), rows);
    (black_box(output), rows)
}

// ── the typed cases ──────────────────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = check_rows)]
fn vec_encode(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .encode(&CITY)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = check_rows)]
fn vec_serialize(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .serialize(&CITY)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = csv_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = csv_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = csv_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = csv_state, teardown = check_rows)]
fn csv_serialize(state: CsvState) -> (Vec<u8>, usize) {
    let (mut writer, rows) = state;
    for _ in 0..rows {
        writer
            .serialize(&CITY)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = writer
        .into_inner()
        .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    (black_box(output), rows)
}

library_benchmark_group!(
    name = encode;
    benchmarks = vec, io, push, csv_writer, vec_encode, vec_serialize, csv_serialize
);

// ── narrow and wide: typed emission against direct dynamic emission ───────────
//
// Direct typed emission removed intermediate `ByteRecord` staging from native
// `encode` and Serde `serialize`. These cases pin that: each width writes the same row three
// ways — direct dynamic `emit_slices`, native `encode` and Serde `serialize` —
// so the difference between `*_direct` and `*_encode`/`*_serialize` is exactly
// the typed path's overhead over framing pre-split slices. String fields keep
// float formatting out of it, so what is left is framing and the former
// staging copy. The typed cases must stay within 10% of
// their `*_direct` baseline at both widths.

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = drop_it)]
fn narrow_direct(state: VecState) -> (usize, Vec<u8>) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .emit_slices(&NARROW_FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = emitter.into_inner();
    assert_eq!(
        output.len(),
        rows * corpus_len(&NARROW_FIELDS),
        "benchmark wrote the wrong bytes"
    );
    (black_box(output.len()), output)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = check_rows)]
fn narrow_encode(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .encode(&NARROW)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = check_rows)]
fn narrow_serialize(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .serialize(&NARROW)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = wide_vec_state, teardown = drop_it)]
fn wide_direct(state: VecState) -> (usize, Vec<u8>) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .emit_slices(&WIDE_FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = emitter.into_inner();
    assert_eq!(
        output.len(),
        rows * corpus_len(&WIDE_FIELDS),
        "benchmark wrote the wrong bytes"
    );
    (black_box(output.len()), output)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = wide_vec_state, teardown = check_rows)]
fn wide_encode(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .encode(&WIDE)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_10(args = (10), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_100(args = (100), setup = wide_vec_state, teardown = check_rows)]
#[bench::rows_1000(args = (1000), setup = wide_vec_state, teardown = check_rows)]
fn wide_serialize(state: VecState) -> (Vec<u8>, usize) {
    let (mut emitter, rows) = state;
    for _ in 0..rows {
        emitter
            .serialize(&WIDE)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    (black_box(emitter.into_inner()), rows)
}

// ── the field-at-a-time builder, against the same rows written as slices ─────
//
// `begin_record`/`write_field`/`finish` is the third way to write a record, and
// the only one that does not know the whole record up front. It stages the
// fields in a `ByteRecord` and then emits that record, so it pays a copy of
// every field's bytes plus the record's bookkeeping on top of what `*_direct`
// pays to frame the same slices. Both widths are here because the staging cost
// is per field: the wide row is where it shows.
//
// | Case            | Direct | Builder | Ratio |
// |-----------------|--------|---------|-------|
// | narrow (2)      |    333 |     494 | 1.48x |
// | wide (12)       |   1611 |    2242 | 1.39x |
//
// Marginal instructions per record, (rows_1000 - rows_100) / 900.
//
// The staging record is held on the emitter and handed to each guard rather
// than allocated per record. Before that reuse these rows cost 865 and 4996,
// so the pair of allocations and their growth reallocations were 43% and 55%
// of the builder's price — more than the field copies they staged. What is
// left is the copy itself and the record's bookkeeping, which is what asking
// for a field-at-a-time API costs and cannot be given back.

#[library_benchmark]
#[bench::rows_1(args = (1), setup = vec_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = vec_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = vec_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = vec_state, teardown = drop_it)]
fn narrow_builder(state: VecState) -> (usize, Vec<u8>) {
    let (mut emitter, rows) = state;
    let written = build_rows(&mut emitter, rows, &NARROW_FIELDS);
    (black_box(written), emitter.into_inner())
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = wide_vec_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = wide_vec_state, teardown = drop_it)]
fn wide_builder(state: VecState) -> (usize, Vec<u8>) {
    let (mut emitter, rows) = state;
    let written = build_rows(&mut emitter, rows, &WIDE_FIELDS);
    (black_box(written), emitter.into_inner())
}

library_benchmark_group!(
    name = staging;
    benchmarks =
        narrow_direct,
        narrow_encode,
        narrow_serialize,
        narrow_builder,
        wide_direct,
        wide_encode,
        wide_serialize,
        wide_builder
);

// ── Activated quoting and escaping policies ─────────────────────────────────

const MODE_ROWS: usize = 1000;

static NECESSARY_FIELDS: [&[u8]; 4] = [b"plain", b"a,b", b"say \"hi\"", b"line\nbreak"];
static ALWAYS_FIELDS: [&[u8]; 2] = [b"plain", b"42"];
static NEVER_FIELDS: [&[u8]; 2] = [b"plain", b"42"];
static NON_NUMERIC_FIELDS: [&[u8]; 2] = [b"42", b"plain"];
static RAW_FIELDS: [&[u8]; 2] = [b"a,b", b"x\"y"];
static DOUBLE_QUOTE_FIELDS: [&[u8]; 1] = [b"say \"hi\""];
static BACKSLASH_FIELDS: [&[u8]; 1] = [b"say \"hi\\there\""];
static MYSQL_FIELDS: [&[u8]; 2] = [b"say \"hi\"", b"line\nbreak"];
static UNQUOTED_FIELDS: [&[u8]; 2] = [b"a,b", b"line\nbreak"];
static ACTIVATED_FIELDS: [&[u8]; 2] = [b"a,b", b"say \"hi\""];

type ModeInput = (FormatOptions, &'static [&'static [u8]], &'static [u8]);
type ModeState = (VecEmitter<Dynamic>, &'static [&'static [u8]], &'static [u8]);

fn mode_state(input: ModeInput) -> ModeState {
    let (format, fields, expected) = input;
    let emitter = VecEmitter::<Dynamic>::with_options(
        Vec::with_capacity(expected.len() * MODE_ROWS),
        format,
        options(),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, fields, expected)
}

fn run_mode(state: ModeState) -> Vec<u8> {
    let (mut emitter, fields, expected) = state;
    for _ in 0..MODE_ROWS {
        emitter
            .emit_slices(fields)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    let output = emitter.into_inner();
    assert_eq!(output.len(), expected.len() * MODE_ROWS);
    for row in output.chunks_exact(expected.len()) {
        assert_eq!(row, expected, "benchmark wrote the wrong bytes");
    }
    black_box(output)
}

macro_rules! mode_case {
    ($name:ident, $format:expr, $fields:expr, $expected:expr) => {
        #[library_benchmark]
        #[bench::rows_1000(
                                                            args = (($format, &$fields, $expected)),
                                                            setup = mode_state,
                                                            teardown = drop_it
                                                        )]
        fn $name(state: ModeState) -> Vec<u8> {
            run_mode(state)
        }
    };
}

mode_case!(
    quoting_necessary,
    FormatOptions::CSV.quoting(Quoting::Necessary),
    NECESSARY_FIELDS,
    b"plain,\"a,b\",\"say \"\"hi\"\"\",\"line\nbreak\"\n"
);
mode_case!(
    quoting_always,
    FormatOptions::CSV.quoting(Quoting::Always),
    ALWAYS_FIELDS,
    b"\"plain\",\"42\"\n"
);
mode_case!(
    quoting_never,
    FormatOptions::CSV.quoting(Quoting::Never),
    NEVER_FIELDS,
    b"plain,42\n"
);
// `is_numeric` in `emit.rs` decides every field this case writes: a byte
// scan recognizes the plain `[sign] digits [. digits]` shape without a UTF-8
// validation or an `f64` parse, falling back to the exact parse for anything
// else (exponents, `inf`/`nan`, and so on). Measured on this row at 1000
// rows, that scan drops the case from 850,422 to 626,534 Callgrind
// instructions — 26% — because most of `NON_NUMERIC_FIELDS` is plain text
// the scan rejects in the first byte it reads.
mode_case!(
    quoting_non_numeric,
    FormatOptions::CSV.quoting(Quoting::NonNumeric),
    NON_NUMERIC_FIELDS,
    b"42,\"plain\"\n"
);
mode_case!(
    quoting_raw,
    FormatOptions::CSV.quoting(Quoting::Raw),
    RAW_FIELDS,
    b"a,b,x\"y\n"
);
mode_case!(
    escape_double_quote,
    FormatOptions::CSV.escape(Escape::DoubleQuote),
    DOUBLE_QUOTE_FIELDS,
    b"\"say \"\"hi\"\"\"\n"
);
mode_case!(
    escape_backslash,
    FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
    BACKSLASH_FIELDS,
    b"\"say \\\"hi\\\\there\\\"\"\n"
);
mode_case!(
    escape_mysql,
    FormatOptions::CSV
        .escape(Escape::Mysql)
        .quoting(Quoting::Never),
    MYSQL_FIELDS,
    b"say \\\"hi\\\",line\\nbreak\n"
);
mode_case!(
    escape_unquoted,
    FormatOptions::CSV
        .escape(Escape::Unquoted(b'\\'))
        .quoting(Quoting::Never),
    UNQUOTED_FIELDS,
    b"a\\,b,line\\\nbreak\n"
);

type TypedModeState = (VecEmitter<Dynamic>, &'static [u8]);

fn typed_mode_state(_: ()) -> TypedModeState {
    let expected = b"\"a,b\",\"say \"\"hi\"\"\"\n".as_slice();
    let emitter = VecEmitter::<Dynamic>::with_options(
        Vec::with_capacity(expected.len() * MODE_ROWS),
        FormatOptions::CSV,
        options(),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (emitter, expected)
}

fn check_typed_mode(output: Vec<u8>, expected: &[u8]) -> Vec<u8> {
    assert_eq!(output.len(), expected.len() * MODE_ROWS);
    for row in output.chunks_exact(expected.len()) {
        assert_eq!(row, expected, "benchmark wrote the wrong bytes");
    }
    black_box(output)
}

#[library_benchmark]
#[bench::rows_1000(args = (()), setup = typed_mode_state, teardown = drop_it)]
fn activated_raw(state: TypedModeState) -> Vec<u8> {
    let (mut emitter, expected) = state;
    for _ in 0..MODE_ROWS {
        emitter
            .emit_slices(&ACTIVATED_FIELDS)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    check_typed_mode(emitter.into_inner(), expected)
}

#[library_benchmark]
#[bench::rows_1000(args = (()), setup = typed_mode_state, teardown = drop_it)]
fn activated_native(state: TypedModeState) -> Vec<u8> {
    let (mut emitter, expected) = state;
    for _ in 0..MODE_ROWS {
        emitter
            .encode(&ACTIVATED)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    check_typed_mode(emitter.into_inner(), expected)
}

#[library_benchmark]
#[bench::rows_1000(args = (()), setup = typed_mode_state, teardown = drop_it)]
fn activated_serde(state: TypedModeState) -> Vec<u8> {
    let (mut emitter, expected) = state;
    for _ in 0..MODE_ROWS {
        emitter
            .serialize(&ACTIVATED)
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
    }
    check_typed_mode(emitter.into_inner(), expected)
}

library_benchmark_group!(
    name = activated;
    benchmarks =
        quoting_necessary,
        quoting_always,
        quoting_never,
        quoting_non_numeric,
        quoting_raw,
        escape_double_quote,
        escape_backslash,
        escape_mysql,
        escape_unquoted,
        activated_raw,
        activated_native,
        activated_serde
);

// ── Scripted sinks: deterministic drain and retry behavior ───────────────────

const SINK_BUFFER: usize = 8 * 1024;
const PARTIAL_WRITE: usize = 1024;

#[derive(Clone, Copy)]
enum SinkShape {
    Small,
    Threshold,
    Oversized,
}

#[derive(Clone, Copy)]
enum SinkMode {
    Full,
    Partial,
}

#[derive(Clone, Copy)]
enum SinkOperation {
    IoEmitter,
    EncodeToWriter,
}

#[derive(CsvEncode)]
struct SinkRow {
    value: String,
}

struct ScriptedSink {
    bytes: Vec<u8>,
    offered: Vec<usize>,
    accepted: Vec<usize>,
    flushes: usize,
    limit: usize,
}

impl ScriptedSink {
    fn new(limit: usize, output_len: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(output_len),
            offered: Vec::new(),
            accepted: Vec::new(),
            flushes: 0,
            limit,
        }
    }
}

impl Write for ScriptedSink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let accepted = buffer.len().min(self.limit);
        self.offered.push(buffer.len());
        self.accepted.push(accepted);
        self.bytes.extend_from_slice(&buffer[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

struct SinkState {
    rows: Vec<SinkRow>,
    expected: Vec<u8>,
    drains: Vec<usize>,
    mode: SinkMode,
    operation: SinkOperation,
}

struct SinkResult {
    sink: ScriptedSink,
    expected: Vec<u8>,
    drains: Vec<usize>,
}

fn sink_rows(shape: SinkShape) -> Vec<SinkRow> {
    let widths: Vec<usize> = match shape {
        SinkShape::Small => vec![2048],
        SinkShape::Threshold => vec![96; 200],
        // The first oversized drain releases its excess capacity. The second
        // retains it for reuse, two ordinary drains release it one generation
        // later, and the final oversized row grows the buffer again.
        SinkShape::Oversized => {
            let mut widths = vec![16 * 1024, 16 * 1024];
            widths.extend([63; 256]);
            widths.push(16 * 1024);
            widths
        }
    };
    widths
        .into_iter()
        .enumerate()
        .map(|(index, width)| SinkRow {
            value: char::from(b'a' + u8::try_from(index % 26).expect("bounded"))
                .to_string()
                .repeat(width),
        })
        .collect()
}

fn sink_state(input: (SinkShape, SinkMode, SinkOperation)) -> SinkState {
    let (shape, mode, operation) = input;
    let rows = sink_rows(shape);
    let mut expected = Vec::new();
    let mut drains = Vec::new();
    let mut buffered = 0;
    for row in &rows {
        expected.extend_from_slice(row.value.as_bytes());
        expected.push(b'\n');
        buffered += row.value.len() + 1;
        if buffered >= SINK_BUFFER {
            drains.push(buffered);
            buffered = 0;
        }
    }
    if buffered != 0 {
        drains.push(buffered);
    }
    SinkState {
        rows,
        expected,
        drains,
        mode,
        operation,
    }
}

fn expected_writes(drains: &[usize], limit: usize) -> (Vec<usize>, Vec<usize>) {
    let mut offered = Vec::new();
    let mut accepted = Vec::new();
    for &drain in drains {
        let mut remaining = drain;
        while remaining != 0 {
            offered.push(remaining);
            let count = remaining.min(limit);
            accepted.push(count);
            remaining -= count;
        }
    }
    (offered, accepted)
}

fn check_sink(result: SinkResult) {
    let (offered, accepted) = expected_writes(&result.drains, result.sink.limit);
    assert_eq!(
        result.sink.bytes, result.expected,
        "scripted sink must receive the exact encoded document"
    );
    assert_eq!(
        result.sink.offered, offered,
        "scripted sink must observe the exact drain/retry request lengths"
    );
    assert_eq!(
        result.sink.accepted, accepted,
        "scripted sink must accept the exact full or bounded-partial lengths"
    );
    assert_eq!(
        result.sink.flushes, 1,
        "the public operation must flush the sink exactly once"
    );
}

#[library_benchmark]
#[bench::io_full_small(
    args = ((SinkShape::Small, SinkMode::Full, SinkOperation::IoEmitter)),
    setup = sink_state,
    teardown = check_sink
)]
#[bench::io_partial_small(
    args = ((SinkShape::Small, SinkMode::Partial, SinkOperation::IoEmitter)),
    setup = sink_state,
    teardown = check_sink
)]
#[bench::encode_full_threshold(
    args = ((SinkShape::Threshold, SinkMode::Full, SinkOperation::EncodeToWriter)),
    setup = sink_state,
    teardown = check_sink
)]
#[bench::encode_partial_threshold(
    args = ((SinkShape::Threshold, SinkMode::Partial, SinkOperation::EncodeToWriter)),
    setup = sink_state,
    teardown = check_sink
)]
#[bench::io_full_oversized(
    args = ((SinkShape::Oversized, SinkMode::Full, SinkOperation::IoEmitter)),
    setup = sink_state,
    teardown = check_sink
)]
#[bench::encode_partial_oversized(
    args = ((SinkShape::Oversized, SinkMode::Partial, SinkOperation::EncodeToWriter)),
    setup = sink_state,
    teardown = check_sink
)]
fn sink_drain(state: SinkState) -> SinkResult {
    let limit = match state.mode {
        SinkMode::Full => usize::MAX,
        SinkMode::Partial => PARTIAL_WRITE,
    };
    let sink = ScriptedSink::new(limit, state.expected.len());
    let sink = match state.operation {
        SinkOperation::IoEmitter => {
            let mut emitter = IoEmitter::with_options(
                sink,
                FormatOptions::CSV,
                EmitOptions::new()
                    .has_headers(false)
                    .buffer_capacity(SINK_BUFFER),
            )
            .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
            for row in black_box(&state.rows) {
                emitter
                    .emit_record([row.value.as_bytes()])
                    .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
            }
            emitter
                .into_inner()
                .unwrap_or_else(|error| panic!("benchmark output failed: {error}"))
        }
        SinkOperation::EncodeToWriter => {
            let mut sink = sink;
            encode_to_writer(
                &mut sink,
                black_box(state.rows),
                FormatOptions::CSV,
                EmitOptions::new()
                    .has_headers(false)
                    .buffer_capacity(SINK_BUFFER),
            )
            .unwrap_or_else(|error| panic!("benchmark output failed: {error}"));
            sink
        }
    };
    SinkResult {
        sink: black_box(sink),
        expected: state.expected,
        drains: state.drains,
    }
}

library_benchmark_group!(
    name = sink_backed;
    benchmarks = sink_drain
);

main!(
    library_benchmark_groups = encode,
    staging,
    activated,
    sink_backed
);
