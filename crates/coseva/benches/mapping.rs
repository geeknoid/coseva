//! What resolving a field mapping and skipping unnamed columns cost as the
//! header widens, the name-resolution and unnamed-column axes move.
//!
//! Three questions live here, each in its own group, because each is a different
//! kind of work and mixing them would blur both.
//!
//! # `mapping`: resolving names against a wide header
//!
//! Resolving each of `k` named fields by scanning all `n` headers costs
//! `k × n` — quadratic in the width when a struct grows with its data. Past a
//! measured product the resolver hashes the headers once and probes instead, so
//! naming every column is linear in the width rather than quadratic.
//!
//! [`FieldProjection::from_headers`] is the public face of that decision — it
//! runs the same width test and the same hash-probe the typed-decode mapping
//! runs — so it is the vehicle here, parameterized by width without hand-writing
//! a two-hundred-field struct for every point.
//!
//! * `map_all` names every column, the shape the scan is quadratic in. At 8
//!   columns the product stays under the threshold and it scans; at 64, 100 and
//!   200 it crosses and resolves through the index. Read down its rows *per
//!   column*: an indexed resolution that is linear in the width holds a flat
//!   per-column cost as the width grows, where a scan's per-column cost climbs
//!   with it. This is the row that shows the quadratic does not bite.
//! * `map_one` names a single column, one scan across the whole header — the
//!   cost of touching every header once, without the per-name work the wide
//!   struct multiplies. It is the linear reference the all-fields row is read
//!   against.
//! * `map_two` names the first and last column, the sparse projection that must
//!   *stay* on the scan: `2 × n` never approaches the threshold, so this is the
//!   guard that widening the fast path did not slow the two-of-many case down.
//!   Its per-name scan cost climbing with the width is the very slope the
//!   indexed `map_all` flattens, read here on a case that keeps the scan.
//!   One- and two-name scans consume bounded header iterators directly and
//!   allocate only their returned projection. Removing their temporary
//!   collections saves 37–42% on `map_one` and 24–29% on `map_two`; removing
//!   the redundant borrowed-slice copies from the collected fallback saves
//!   1–10% on `map_all`.
//!
//! # `wide_select`: skipping unnamed columns past the learned word
//!
//! The Serde struct path learns which columns a visitor discards and skips them
//! outright on later records. A single 64-bit word as the learned set cannot
//! hold a column past index 63, so a wide record would re-walk
//! every unnamed column on every row. A scalable atomic set learns a
//! column at any index, while the single word still serves the common narrow
//! header with no allocation.
//!
//! * `select_two_wide` deserializes two of two hundred columns — `c000` and the
//!   last — so the record learns to skip a hundred and ninety-eight columns, all
//!   but two of them past the first word. Read `rows_1000` against `rows_100`:
//!   the per-record figure is their difference over 900, which cancels the
//!   one-time learning the first record pays.
//! * `select_two_narrow` deserializes two of six columns, entirely within the
//!   first word, so it never touches the wide set. It is the guard that the
//!   scalable set did not cost the narrow header anything.
//!
//! # `projection`: applying a resolved projection to every record
//!
//! Name resolution happens once, but a projection is applied to every record.
//! These cases reuse a two-column projection over a two-hundred-column record
//! and consume both fields through the owned byte, owned text and lending byte
//! surfaces. The owned cases isolate projection traversal; the lending case
//! includes the parser work required to create a valid borrowed `Record`, so it
//! protects that end-to-end public path. Read `rows_1000` against `rows_100`:
//! their difference over 900 is the steady-state cost per record with setup and
//! resolution cancelled.
//!
//! # Reading the numbers
//!
//! Callgrind instruction counts. Within `mapping` each row is one resolution,
//! so the rows compare directly. Within `wide_select` and `projection`, per
//! record is `(rows_1000 - rows_100) / 900`, the same fixed-cost cancellation
//! the other width suites use. As every file here warns, numbers are comparable
//! within this table and not across files.

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

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, FieldProjection, SliceParser, TextRecord};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(dead_code, reason = "this file builds its own header-swept corpus")]
mod fixture;

use fixture::drop_it;

/// The width of a column name, `cNNN`, fixed so a name costs the same at every
/// width in the sweep, matching the other width suites' corpus.
const NAME_LEN: usize = 4;

/// The width of every field value, so a record is a fixed size and only the
/// column count varies.
const VALUE_LEN: usize = 5;

/// The value column `index` carries, at every width, the same corpus the other
/// width suites use so a column holds identical bytes at every width.
const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

// ── Resolving names against a wide header ────────────────────────────────────

/// A header record `c000..cNNN` of the requested width, built owned in setup so
/// only the resolution that follows is measured.
fn header_record(width: usize) -> ByteRecord {
    ByteRecord::from(
        (0..width)
            .map(|column| format!("c{column:03}").into_bytes())
            .collect::<Vec<_>>(),
    )
}

/// A header and every one of its column names, the all-fields shape that is
/// quadratic before the indexed path.
fn all_state(width: usize) -> (ByteRecord, Vec<String>) {
    (
        header_record(width),
        (0..width).map(|column| format!("c{column:03}")).collect(),
    )
}

/// A header and a single column name, one scan of the whole header.
fn one_state(width: usize) -> (ByteRecord, Vec<String>) {
    (header_record(width), vec![format!("c{:03}", width - 1)])
}

/// A header and its first and last column name, the sparse projection that must
/// stay on the scan.
fn two_state(width: usize) -> (ByteRecord, Vec<String>) {
    (
        header_record(width),
        vec!["c000".to_owned(), format!("c{:03}", width - 1)],
    )
}

/// Resolve `names` against `headers`, the one thing every `mapping` case
/// measures.
fn resolve(state: (ByteRecord, Vec<String>)) -> FieldProjection {
    let (headers, names) = state;
    FieldProjection::from_headers(&headers, names.iter().map(String::as_str))
        .unwrap_or_else(|error| panic!("benchmark projection failed: {error}"))
}

#[library_benchmark]
#[bench::w8(args = (8_usize), setup = all_state, teardown = drop_it)]
#[bench::w64(args = (64_usize), setup = all_state, teardown = drop_it)]
#[bench::w100(args = (100_usize), setup = all_state, teardown = drop_it)]
#[bench::w200(args = (200_usize), setup = all_state, teardown = drop_it)]
fn map_all(state: (ByteRecord, Vec<String>)) -> FieldProjection {
    resolve(state)
}

#[library_benchmark]
#[bench::w64(args = (64_usize), setup = one_state, teardown = drop_it)]
#[bench::w100(args = (100_usize), setup = one_state, teardown = drop_it)]
#[bench::w200(args = (200_usize), setup = one_state, teardown = drop_it)]
fn map_one(state: (ByteRecord, Vec<String>)) -> FieldProjection {
    resolve(state)
}

#[library_benchmark]
#[bench::w100(args = (100_usize), setup = two_state, teardown = drop_it)]
#[bench::w200(args = (200_usize), setup = two_state, teardown = drop_it)]
fn map_two(state: (ByteRecord, Vec<String>)) -> FieldProjection {
    resolve(state)
}

// ── Skipping unnamed columns past the learned word ───────────────────────────

type SliceState = (SliceParser<'static, Csv>, &'static [u8]);

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::FirstRecord)
}

/// Build one width's corpus, its two-column decode target, and its measured
/// bodies.
///
/// A macro rather than const generics because the decode target names its own
/// last column, and a field name cannot be computed from a const parameter.
macro_rules! wide {
    ($module:ident, $columns:literal, $last:ident, $last_index:literal) => {
        mod $module {
            use super::*;

            const COLUMNS: usize = $columns;
            const HEADER_LEN: usize = COLUMNS * (NAME_LEN + 1);
            const ROW_LEN: usize = COLUMNS * (VALUE_LEN + 1);

            const fn header() -> [u8; HEADER_LEN] {
                let mut out = [0_u8; HEADER_LEN];
                let mut index = 0;
                while index < COLUMNS {
                    let base = index * (NAME_LEN + 1);
                    out[base] = b'c';
                    out[base + 1] = b'0' + (index / 100) as u8;
                    out[base + 2] = b'0' + ((index / 10) % 10) as u8;
                    out[base + 3] = b'0' + (index % 10) as u8;
                    out[base + 4] = if index + 1 == COLUMNS { b'\n' } else { b',' };
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

            static BUF_100: [u8; HEADER_LEN + ROW_LEN * 100] = corpus();
            static BUF_1000: [u8; HEADER_LEN + ROW_LEN * 1000] = corpus();

            pub(super) static ROWS_100: &[u8] = &BUF_100;
            pub(super) static ROWS_1000: &[u8] = &BUF_1000;

            /// The first and last column of this width, borrowed rather than
            /// owned so no case allocates per record.
            #[derive(serde::Deserialize)]
            struct Pick<'input> {
                c000: &'input str,
                $last: u64,
            }

            /// The checksum one record contributes: the length of the borrowed
            /// first column plus the parsed last one.
            const PER_RECORD: u64 = VALUE_LEN as u64 + value($last_index);

            pub(super) fn rows_in(input: &[u8]) -> u64 {
                ((input.len() - HEADER_LEN) / ROW_LEN) as u64
            }

            /// Assert the case deserialized the intended two columns of every
            /// record, so a mapping that resolved to the wrong offsets cannot
            /// still look comparable.
            fn check(total: u64, input: &[u8]) -> u64 {
                let expected = rows_in(input) * PER_RECORD;
                assert_eq!(total, expected, "benchmark deserialized the wrong fields");
                total
            }

            pub(super) fn state(input: &'static [u8]) -> SliceState {
                let parser = SliceParser::<Csv>::new(input, options())
                    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
                (parser, input)
            }

            pub(super) fn run(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
                let (mut parser, input) = state;
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    let row: Pick<'_> = line
                        .deserialized()
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(row.c000.len() as u64 + row.$last);
                }
                (black_box(check(total, input)), parser)
            }
        }
    };
}

wide!(narrow, 6, c005, 5);
wide!(wide200, 200, c199, 199);

#[library_benchmark]
#[bench::rows_100(args = (narrow::ROWS_100), setup = narrow::state, teardown = drop_it)]
#[bench::rows_1000(args = (narrow::ROWS_1000), setup = narrow::state, teardown = drop_it)]
fn select_two_narrow(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    narrow::run(state)
}

#[library_benchmark]
#[bench::rows_100(args = (wide200::ROWS_100), setup = wide200::state, teardown = drop_it)]
#[bench::rows_1000(args = (wide200::ROWS_1000), setup = wide200::state, teardown = drop_it)]
fn select_two_wide(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
    wide200::run(state)
}

// ── Applying a resolved projection to every record ──────────────────────────

const PROJECTION_COLUMNS: usize = 200;

fn projection_bytes() -> Vec<Vec<u8>> {
    (0..PROJECTION_COLUMNS)
        .map(|column| value(column).to_string().into_bytes())
        .collect()
}

fn projection_checksum<'field>(fields: impl Iterator<Item = Option<&'field [u8]>>) -> u64 {
    let mut checksum = 0_u64;
    for field in fields {
        for &byte in field.unwrap_or_else(|| panic!("benchmark projection missed a field")) {
            checksum = checksum.wrapping_mul(257).wrapping_add(byte as u64);
        }
        checksum = checksum.wrapping_mul(257).wrapping_add(u64::MAX);
    }
    checksum
}

fn check_projection(total: u64, rows: usize) -> u64 {
    let expected = projection_checksum([Some(&b"10000"[..]), Some(&b"37263"[..])].into_iter());
    assert_eq!(
        total,
        expected.wrapping_mul(rows as u64),
        "benchmark projected the wrong fields"
    );
    total
}

type ByteProjectionState = (Vec<ByteRecord>, FieldProjection);

fn byte_projection_state(rows: usize) -> ByteProjectionState {
    let record = ByteRecord::from(projection_bytes());
    (
        vec![record; rows],
        FieldProjection::new([0, PROJECTION_COLUMNS - 1]),
    )
}

#[library_benchmark]
#[bench::rows_100(args = (100_usize), setup = byte_projection_state, teardown = drop_it)]
#[bench::rows_1000(args = (1_000_usize), setup = byte_projection_state, teardown = drop_it)]
fn project_byte_record(state: ByteProjectionState) -> (u64, Vec<ByteRecord>, FieldProjection) {
    let (records, projection) = state;
    let mut total = 0_u64;
    for record in &records {
        total = total.wrapping_add(projection_checksum(record.project(&projection)));
    }
    (
        black_box(check_projection(total, records.len())),
        records,
        projection,
    )
}

type TextProjectionState = (Vec<TextRecord>, FieldProjection);

fn text_projection_state(rows: usize) -> TextProjectionState {
    let fields = projection_bytes()
        .into_iter()
        .map(|field| {
            String::from_utf8(field)
                .unwrap_or_else(|error| panic!("invalid benchmark field: {error}"))
        })
        .collect::<Vec<_>>();
    let record = TextRecord::from(fields);
    (
        vec![record; rows],
        FieldProjection::new([0, PROJECTION_COLUMNS - 1]),
    )
}

#[library_benchmark]
#[bench::rows_100(args = (100_usize), setup = text_projection_state, teardown = drop_it)]
#[bench::rows_1000(args = (1_000_usize), setup = text_projection_state, teardown = drop_it)]
fn project_text_record(state: TextProjectionState) -> (u64, Vec<TextRecord>, FieldProjection) {
    let (records, projection) = state;
    let mut total = 0_u64;
    for record in &records {
        total = total.wrapping_add(projection_checksum(
            record
                .project(&projection)
                .map(|field| field.map(str::as_bytes)),
        ));
    }
    (
        black_box(check_projection(total, records.len())),
        records,
        projection,
    )
}

type LendingProjectionState = (SliceParser<'static, Csv>, FieldProjection, &'static [u8]);

fn lending_projection_state(input: &'static [u8]) -> LendingProjectionState {
    (
        SliceParser::<Csv>::new(input, options())
            .unwrap_or_else(|error| panic!("invalid benchmark config: {error}")),
        FieldProjection::new([0, PROJECTION_COLUMNS - 1]),
        input,
    )
}

#[library_benchmark]
#[bench::rows_100(args = (wide200::ROWS_100), setup = lending_projection_state, teardown = drop_it)]
#[bench::rows_1000(args = (wide200::ROWS_1000), setup = lending_projection_state, teardown = drop_it)]
fn project_lending_record(
    state: LendingProjectionState,
) -> (u64, SliceParser<'static, Csv>, FieldProjection) {
    let (mut parser, projection, input) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        let record = line
            .record()
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(projection_checksum(record.project(&projection)));
    }
    let rows = wide200::rows_in(input) as usize;
    (black_box(check_projection(total, rows)), parser, projection)
}

library_benchmark_group!(
    name = mapping;
    benchmarks = map_all, map_one, map_two
);

library_benchmark_group!(
    name = wide_select;
    benchmarks = select_two_narrow, select_two_wide
);

library_benchmark_group!(
    name = projection;
    benchmarks = project_byte_record, project_text_record, project_lending_record
);

main!(library_benchmark_groups = mapping, wide_select, projection);
