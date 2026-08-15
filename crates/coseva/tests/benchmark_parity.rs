//! Proof that the published benchmark comparisons measure what they claim to.
//!
//! A performance comparison is only worth printing if both sides do the same
//! work. That property does not survive refactoring on its own: a front end
//! configured to skip headers while its counterpart resolves them, a case that
//! stops reading a column, or an unescaping path that quietly returns the raw
//! bytes would all still produce a plausible table, and the number would be
//! smaller for a reason that has nothing to do with speed.
//!
//! Every benchmark already asserts its own checksum through a `check` function,
//! so a divergence is caught the moment a suite runs. But the suites run under
//! Callgrind, which means they run neither in `cargo test` nor in CI, so in
//! practice nothing checks them between one deliberate measuring session and
//! the next. This file is what closes that window.
//!
//! # What is shared with the benchmarks, and what is restated
//!
//! `benches/fixture.rs` is pulled in by `#[path]`, so the corpus, the field
//! widths and the expected-value formula are the *same code* the suites use. A
//! change to the corpus that invalidated a comparison would fail here without
//! anyone remembering to update this file, which is the property worth having:
//! the corpus is where a silent divergence actually hides, because both sides
//! read it and neither mentions it.
//!
//! The measured bodies themselves are restated rather than shared, because they
//! live behind `#[library_benchmark]` in binaries that cannot be imported. What
//! is asserted is therefore not "the benchmark body is correct" but the weaker
//! and still useful "the workload each pair describes agrees across all four
//! sides and matches the fixture's own formula". Where a suite pins a
//! configuration that the comparison depends on — headers off, a shared buffer
//! capacity — this file pins the same one and says so.
//!
//! # What each test pins down
//!
//! - **Record counts.** coseva and `csv` must consume the same header row and
//!   yield the same number of records.
//! - **Checksums.** Every side of a pair must return a bit-identical value, and
//!   that value is additionally checked against `fixture`'s own formula, so a
//!   bug shared by both sides still fails.
//! - **Unescaping.** The `quoted` suite's three corpora carry the same fields
//!   written three ways, so a correct parser returns one checksum for all
//!   three. A case that skipped unescaping would return a larger one.
//! - **Shortcut equivalence.** Predicate pushdown must find exactly the records
//!   the same scan finds by hand, which is what `filter`'s `manual` row claims.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![expect(
    clippy::expect_used,
    reason = "an invalid benchmark fixture must fail loudly"
)]

use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, Column, IoParser, Predicate, PushParser, SliceParser, TextRecord};

#[path = "../benches/fixture.rs"]
#[expect(dead_code, reason = "the fixture serves several benchmark targets")]
mod fixture;

use fixture::{BUFFER, FIELD_BYTES, FIELDS, ROW_LEN, ROWS_1, ROWS_10, ROWS_100, ROWS_1000};

/// The parse options every headerless record suite pins.
///
/// `byte_record`, `text_record`, `quoted` and `filter` all build exactly this.
/// The buffer capacity is set explicitly on both sides of every comparison so
/// that a pair cannot quietly become a comparison of default buffer sizes.
fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(BUFFER)
}

/// The `csv` reader every headerless comparison pins, matching [`options`].
fn csv_reader(input: &'static [u8]) -> ::csv::Reader<Cursor<&'static [u8]>> {
    ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(input))
}

/// Every corpus size the record suites measure.
fn corpora() -> [&'static [u8]; 4] {
    [ROWS_1, ROWS_10, ROWS_100, ROWS_1000]
}

fn assert_text_record_parity(input: &[u8], headers: Headers, context: &str) {
    let has_headers = headers == Headers::FirstRecord;
    let options = ParseOptions::new().headers(headers).buffer_capacity(BUFFER);
    let mut parser = SliceParser::<Csv>::new(input, options).expect("coseva parser");
    let mut reader = ::csv::ReaderBuilder::new()
        .has_headers(has_headers)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(input));
    let mut text = TextRecord::new();
    let mut csv = ::csv::StringRecord::new();
    let mut record_index = 0_u64;

    loop {
        let line = parser.next_line().expect("coseva record");
        let has_csv = reader.read_record(&mut csv).expect("csv record");
        assert_eq!(
            line.is_some(),
            has_csv,
            "{context}: parsers disagree at record {record_index}"
        );
        let Some(mut line) = line else {
            break;
        };

        line.read_text_record_into(&mut text)
            .expect("valid UTF-8 record");
        assert_eq!(
            text.len(),
            csv.len(),
            "{context}: field count differs at record {record_index}"
        );
        for field_index in 0..text.len() {
            assert_eq!(
                text.get(field_index).map(str::as_bytes),
                csv.get(field_index).map(str::as_bytes),
                "{context}: field {field_index} differs at record {record_index}"
            );
        }
        record_index += 1;
    }
}

/// The total decoded field length and record count a side reported.
#[derive(Debug, PartialEq, Eq)]
struct Walked {
    bytes: u64,
    records: u64,
}

fn sum_byte_record(record: &ByteRecord) -> u64 {
    (0..record.len()).fold(0_u64, |total, index| {
        total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64)
    })
}

// ── the four sides of a record-shape comparison ──────────────────────────────

fn slice_side(input: &'static [u8]) -> Walked {
    let mut parser = SliceParser::<Csv>::new(input, options()).expect("parser");
    let mut record = ByteRecord::with_capacity(FIELDS, ROW_LEN);
    let mut walked = Walked {
        bytes: 0,
        records: 0,
    };
    while let Some(mut line) = parser.next_line().expect("parse") {
        line.read_byte_record_into(&mut record).expect("record");
        walked.bytes = walked.bytes.wrapping_add(sum_byte_record(&record));
        walked.records += 1;
    }
    walked
}

fn io_side(input: &'static [u8]) -> Walked {
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input), options()).expect("parser");
    let mut record = ByteRecord::with_capacity(FIELDS, ROW_LEN);
    let mut walked = Walked {
        bytes: 0,
        records: 0,
    };
    while let Some(mut line) = parser.next_line().expect("parse") {
        line.read_byte_record_into(&mut record).expect("record");
        walked.bytes = walked.bytes.wrapping_add(sum_byte_record(&record));
        walked.records += 1;
    }
    walked
}

fn drain(chunk: &mut Chunk<'_, '_, Csv>, record: &mut ByteRecord, walked: &mut Walked) {
    while chunk.read_byte_record_into(record).expect("record") {
        walked.bytes = walked.bytes.wrapping_add(sum_byte_record(record));
        walked.records += 1;
    }
}

fn push_side(input: &'static [u8]) -> Walked {
    let mut parser = PushParser::<Csv>::new(options()).expect("parser");
    let mut record = ByteRecord::with_capacity(FIELDS, ROW_LEN);
    let mut walked = Walked {
        bytes: 0,
        records: 0,
    };
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..]);
        drain(&mut chunk, &mut record, &mut walked);
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    drain(&mut chunk, &mut record, &mut walked);
    let _ = chunk.done();
    walked
}

fn csv_side(input: &'static [u8]) -> Walked {
    let mut reader = csv_reader(input);
    let mut record = ::csv::ByteRecord::with_capacity(ROW_LEN, FIELDS);
    let mut walked = Walked {
        bytes: 0,
        records: 0,
    };
    while reader.read_byte_record(&mut record).expect("parse") {
        for field in &record {
            walked.bytes = walked.bytes.wrapping_add(field.len() as u64);
        }
        walked.records += 1;
    }
    walked
}

/// Every side of a record-shape comparison walks the same records.
///
/// This is the property the `byte_record`, `text_record` and `read_record`
/// tables rest on: all four rows read the same bytes and reach the same total,
/// so the only thing separating their numbers is how much work each one does to
/// get there.
#[test]
fn record_shape_sides_agree() {
    for input in corpora() {
        assert_text_record_parity(input, Headers::None, "focused TextRecord");
        let records = (input.len() / ROW_LEN) as u64;
        let expected = Walked {
            bytes: records * FIELD_BYTES,
            records,
        };

        // Checked against the fixture's own formula rather than against each
        // other, so a bug present on every side still fails.
        assert_eq!(slice_side(input), expected, "slice, {records} rows");
        assert_eq!(io_side(input), expected, "io, {records} rows");
        assert_eq!(push_side(input), expected, "push, {records} rows");
        assert_eq!(csv_side(input), expected, "csv, {records} rows");
    }
}

// ── the quoted comparison ────────────────────────────────────────────────────

/// The three rows `benches/quoted.rs` measures, written to decode identically.
const PLAIN_ROW: &[u8] = b"Boston,Massachusetts,4500000,42.3601,-71.0589,true\n";
const QUOTED_ROW: &[u8] = b"\"Boston\",\"Massachusetts\",4500000,42.3601,-71.0589,true\n";
const ESCAPED_ROW: &[u8] = b"\"Bo\"\"ton\",\"Ma\"\"sachusetts\",4500000,42.3601,-71.0589,true\n";

fn repeat(row: &[u8], rows: usize) -> &'static [u8] {
    Box::leak(row.repeat(rows).into_boxed_slice())
}

/// Quoting changes the bytes on the wire but not the fields they decode to.
///
/// `benches/quoted.rs` publishes the cost of quoting as the difference between
/// three rows of one table, which is only a cost of *quoting* if all three
/// decode to the same thing. They are built to: `"Boston"` and `"Bo""ton"` are
/// both six characters once unescaped, as is `Boston`. A regression that
/// returned the raw bytes, or that stopped collapsing a doubled quote, would
/// make the quoted rows look cheap and would fail here.
#[test]
fn quoted_corpora_decode_identically() {
    for rows in [1, 10, 100, 1000] {
        let expected = Walked {
            bytes: rows as u64 * FIELD_BYTES,
            records: rows as u64,
        };
        for (name, row) in [
            ("plain", PLAIN_ROW),
            ("quoted", QUOTED_ROW),
            ("escaped", ESCAPED_ROW),
        ] {
            let input = repeat(row, rows);
            assert_eq!(slice_side(input), expected, "{name} slice, {rows} rows");
            assert_eq!(io_side(input), expected, "{name} io, {rows} rows");
            assert_eq!(push_side(input), expected, "{name} push, {rows} rows");
            assert_eq!(csv_side(input), expected, "{name} csv, {rows} rows");
        }
    }
}

// ── the filter comparison ────────────────────────────────────────────────────

const HIT: &[u8] = b"Boston";
const MISS: &[u8] = b"Austin";
const TAIL: &[u8] = b",Massachusetts,4500000,42.3601,-71.0589,true\n";
const NEEDLE: &[u8] = b"osto";
const FILTER_ROWS: usize = 1000;

/// `benches/filter.rs`'s corpus: every `stride`-th record matches.
fn filter_corpus(stride: usize) -> &'static [u8] {
    let mut out = Vec::new();
    for record in 0..FILTER_ROWS {
        out.extend_from_slice(if record % stride == 0 { HIT } else { MISS });
        out.extend_from_slice(TAIL);
    }
    Box::leak(out.into_boxed_slice())
}

/// Count matches through predicate pushdown.
fn filtered(input: &'static [u8], predicate: &Predicate) -> u64 {
    let mut parser = SliceParser::<Csv>::new(input, options()).expect("parser");
    let mut found = 0;
    while parser
        .next_matching_line(predicate)
        .expect("parse")
        .is_some()
    {
        found += 1;
    }
    found
}

/// Count the same matches by reading every record and testing column 0.
fn filtered_manually(input: &'static [u8], test: impl Fn(&[u8]) -> bool) -> u64 {
    let mut parser = SliceParser::<Csv>::new(input, options()).expect("parser");
    let mut record = ByteRecord::with_capacity(FIELDS, ROW_LEN);
    let mut found = 0;
    while let Some(mut line) = parser.next_line().expect("parse") {
        line.read_byte_record_into(&mut record).expect("record");
        if record.get(0).is_some_and(&test) {
            found += 1;
        }
    }
    found
}

/// Pushdown finds exactly the records the same scan finds by hand.
///
/// `benches/filter.rs` publishes `filtered` against `manual` and says plainly
/// that `manual` is the row that matters, because it is the same crate over the
/// same bytes with only the filter changing. That comparison means nothing
/// unless both rows find the same records, which is what this asserts — at
/// every selectivity the suite measures, since a shortcut that is correct when
/// everything matches can still be wrong when almost nothing does.
#[test]
fn pushdown_matches_a_manual_scan() {
    for (stride, expected) in [(1, 1000), (100, 10), (1000, 1)] {
        let input = filter_corpus(stride);

        let equals = filtered(input, &Predicate::equals(0, HIT));
        let equals_manual = filtered_manually(input, |field| field == HIT);
        assert_eq!(equals, expected, "equals pushdown, stride {stride}");
        assert_eq!(equals_manual, expected, "equals manual, stride {stride}");

        let contains = filtered(input, &Predicate::contains(0, NEEDLE));
        let contains_manual = filtered_manually(input, |field| {
            field.windows(NEEDLE.len()).any(|w| w == NEEDLE)
        });
        assert_eq!(contains, expected, "contains pushdown, stride {stride}");
        assert_eq!(
            contains_manual, expected,
            "contains manual, stride {stride}"
        );
    }
}

/// The needle really is present in one city and absent from the other.
///
/// `filter`'s `contains` table only measures what it says if `MISS` fails the
/// search, and it fails it late — `Austin` shares no prefix with `osto`, so the
/// scan runs to the end of the field. If a future edit made `MISS` match, every
/// selectivity in that table would silently become 100%.
#[test]
fn filter_fixture_preconditions_hold() {
    assert_eq!(HIT.len(), MISS.len(), "corpora must be the same size");
    assert!(HIT.windows(NEEDLE.len()).any(|w| w == NEEDLE));
    assert!(!MISS.windows(NEEDLE.len()).any(|w| w == NEEDLE));
    assert_ne!(HIT, MISS);
}

/// A `Column::Name` predicate resolves to the same column an index does.
///
/// The filter suite measures index predicates, but the crate publishes both
/// forms as the same feature, so the named form is checked to find the same
/// records rather than left to a benchmark nobody runs.
#[test]
fn named_and_indexed_predicates_agree() {
    let mut out = Vec::from(&b"city,state,population,latitude,longitude,active\n"[..]);
    out.extend_from_slice(filter_corpus(100));
    let input: &'static [u8] = Box::leak(out.into_boxed_slice());

    let headed = || {
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .buffer_capacity(BUFFER)
    };

    let count = |predicate: Predicate| {
        let mut parser = SliceParser::<Csv>::new(input, headed()).expect("parser");
        let mut found = 0;
        while parser
            .next_matching_line(&predicate)
            .expect("parse")
            .is_some()
        {
            found += 1;
        }
        found
    };

    let by_index = count(Predicate::equals(0, HIT));
    let by_name = count(Predicate::equals(Column::Name("city".into()), HIT));
    assert_eq!(by_index, 10, "the corpus matches every hundredth record");
    assert_eq!(by_name, by_index, "named and indexed must agree");
}

// ── the customer matrix ──────────────────────────────────────────────────────
//
// `benches/matrix.rs` is the source of `docs/PERF.md`, and it publishes three
// coseva-versus-`csv` pairs over five documents. Its own module documentation
// promises that these assertions run without valgrind; this is where that
// promise is kept. As above, the corpus is shared by `#[path]` and the measured
// bodies are restated, so what is pinned is that every side of every published
// pair agrees with a checksum the generator computed and none of them did.

#[path = "../benches/documents.rs"]
mod documents;

use documents::{DOCUMENTS, Document};

/// The configuration every matrix case pins, on both sides.
///
/// The matrix resolves headers where the suites above do not, because a
/// customer reading a real file has a header row. Both crates are given the
/// same buffer for the same reason the headerless comparisons are.
fn matrix_options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::FirstRecord)
        .buffer_capacity(BUFFER)
}

fn matrix_csv_reader(input: &[u8]) -> ::csv::Reader<Cursor<&[u8]>> {
    ::csv::ReaderBuilder::new()
        .has_headers(true)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(input))
}

/// Walk a document with every front end, and assert all three agree.
///
/// Takes the per-line body rather than repeating the three drive loops five
/// times over. The returned checksum is whatever the body accumulated, which
/// each caller then holds against the generator's own figure.
fn walk_every_front_end(
    document: &Document,
    mut body: impl FnMut(&mut coseva::Line<'_, Csv>) -> u64,
) -> u64 {
    let input = &*document.bytes;

    let mut parser = SliceParser::<Csv>::new(input, matrix_options()).expect("parser");
    let mut slice_total = 0_u64;
    while let Some(mut line) = parser.next_line().expect("parse") {
        slice_total = slice_total.wrapping_add(body(&mut line));
    }

    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input), matrix_options()).expect("parser");
    let mut io_total = 0_u64;
    while let Some(mut line) = parser.next_line().expect("parse") {
        io_total = io_total.wrapping_add(body(&mut line));
    }

    // Fed a byte at a time, which the measured version does not do. A push
    // parser that mishandled a record split across a chunk boundary would still
    // measure well at 256 KiB a go; the interesting boundaries are only reached
    // by making every byte a boundary, and this is a test rather than a
    // measurement so it can afford to.
    let mut parser = PushParser::<Csv>::new(matrix_options()).expect("parser");
    let mut push_total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..=fed]);
        while let Some(mut line) = chunk.next_line().expect("parse") {
            push_total = push_total.wrapping_add(body(&mut line));
        }
        fed += 1;
        assert_eq!(chunk.done(), 1, "a one-byte chunk is consumed whole");
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    while let Some(mut line) = chunk.next_line().expect("parse") {
        push_total = push_total.wrapping_add(body(&mut line));
    }
    let _ = chunk.done();

    let name = document.name;
    assert_eq!(slice_total, io_total, "`{name}`: slice and io must agree");
    assert_eq!(
        slice_total, push_total,
        "`{name}`: slice and push must agree"
    );
    slice_total
}

#[test]
fn matrix_record_shapes_agree_with_the_generator() {
    for document in &*DOCUMENTS {
        let name = document.name;

        let borrowed = walk_every_front_end(document, |line| {
            let record = line.record().expect("record");
            record
                .into_iter()
                .fold(0_u64, |total, field| total + field.len() as u64)
        });

        let mut text = TextRecord::new();
        let owned_text = walk_every_front_end(document, |line| {
            line.read_text_record_into(&mut text).expect("record");
            (0..text.len()).fold(0_u64, |total, index| {
                total + text.get(index).map_or(0, str::len) as u64
            })
        });

        let mut bytes = ByteRecord::new();
        let owned_bytes = walk_every_front_end(document, |line| {
            line.read_byte_record_into(&mut bytes).expect("record");
            sum_byte_record(&bytes)
        });

        // The generator accumulated this while writing, so it is not a figure
        // any parser here produced. A bug the three shapes shared still fails.
        assert_eq!(
            borrowed, document.field_bytes,
            "`{name}`: borrowed records read the wrong fields"
        );
        assert_eq!(
            owned_text, document.field_bytes,
            "`{name}`: `TextRecord` disagrees with the borrowed record"
        );
        assert_eq!(
            owned_bytes, document.field_bytes,
            "`{name}`: `ByteRecord` disagrees with the borrowed record"
        );
    }
}

#[test]
fn matrix_publishes_pairs_that_read_the_same_documents() {
    for document in &*DOCUMENTS {
        let name = document.name;
        assert_text_record_parity(&document.bytes, Headers::FirstRecord, name);

        let mut coseva_records = 0_u64;
        let mut text = TextRecord::new();
        let coseva_total = walk_every_front_end(document, |line| {
            line.read_text_record_into(&mut text).expect("record");
            coseva_records += 1;
            (0..text.len()).fold(0_u64, |total, index| {
                total + text.get(index).map_or(0, str::len) as u64
            })
        });
        // Three front ends walked it, so the count is three times over.
        coseva_records /= 3;

        let mut reader = matrix_csv_reader(&document.bytes);
        let mut record = ::csv::StringRecord::new();
        let mut csv_total = 0_u64;
        let mut csv_records = 0_u64;
        while reader.read_record(&mut record).expect("csv record") {
            csv_total += record.iter().map(|f| f.len() as u64).sum::<u64>();
            csv_records += 1;
        }

        let mut reader = matrix_csv_reader(&document.bytes);
        let mut raw = ::csv::ByteRecord::new();
        let mut csv_byte_total = 0_u64;
        while reader.read_byte_record(&mut raw).expect("csv record") {
            csv_byte_total += raw.iter().map(|f| f.len() as u64).sum::<u64>();
        }

        assert_eq!(
            coseva_records, csv_records,
            "`{name}`: both crates must consume the header and yield the same records"
        );
        assert_eq!(
            csv_records, document.records as u64,
            "`{name}`: the report normalises by this count, so it must be right"
        );
        assert_eq!(
            csv_total, coseva_total,
            "`{name}`: the published `StringRecord` pair must do the same work"
        );
        assert_eq!(
            csv_byte_total, document.field_bytes,
            "`{name}`: the published `ByteRecord` pair must do the same work"
        );
    }
}

/// The two typed rows the matrix decodes, present in all five documents.
///
/// Gated because the matrix's typed rows need these features while the record
/// rows above need only `std`, and gating the whole file on them would stop a
/// plain `cargo test` checking any of it.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct SerdeRow {
    value: u64,
}

#[cfg(feature = "serde")]
#[test]
fn matrix_deserialized_pair_decodes_the_same_values() {
    for document in &*DOCUMENTS {
        let name = document.name;

        let coseva_total = walk_every_front_end(document, |line| {
            let row: SerdeRow = line.deserialized().expect("deserialize");
            row.value
        });

        let mut reader = matrix_csv_reader(&document.bytes);
        let mut csv_total = 0_u64;
        for row in reader.deserialize::<SerdeRow>() {
            csv_total += row.expect("csv deserialize").value;
        }

        assert_eq!(
            coseva_total, document.value_sum,
            "`{name}`: deserializing decoded the wrong values"
        );
        assert_eq!(
            csv_total, coseva_total,
            "`{name}`: the published Serde pair must decode the same values"
        );
    }
}

#[cfg(feature = "derive")]
#[derive(coseva::encoding::CsvDecode)]
struct DecodedRow {
    value: u64,
}

/// The decoded shape has no `csv` counterpart, so this holds it against the
/// generator alone. `PERF.md` publishes its numbers, which is reason enough to
/// check it reads what it claims.
#[cfg(feature = "derive")]
#[test]
fn matrix_decoded_shape_decodes_the_same_values() {
    for document in &*DOCUMENTS {
        let total = walk_every_front_end(document, |line| {
            let row: DecodedRow = line.decoded().expect("decode");
            row.value
        });
        assert_eq!(
            total, document.value_sum,
            "`{}`: decoding read the wrong values",
            document.name
        );
    }
}
