//! What a parser costs before it has produced anything, as the header widens.
//!
//! Every other suite here measures per-record work, and reports a `rows_1`
//! column that mixes it with everything a parser does once. That column is not
//! a comparison — front ends resolve headers at different moments, and the
//! `csv` crate parses its header row in `setup`, outside the measured region —
//! so the fixed cost has to be inferred from a number built to measure
//! something else.
//!
//! It is worth measuring directly because startup work is invisible to every
//! per-record column: a `HashMap` built over all hundred header names that
//! nothing goes on to read costs about 138,000 instructions at a hundred
//! columns and moves no per-record number at all. A suite that watches startup
//! on its own axis catches that class of thing on its own terms.
//!
//! # The axis
//!
//! Header count: 6, 20, 60 and 200 columns. Column `i` is named `cNNN`, always
//! four characters, and holds `10000 + i * 137`, always five digits — the same
//! corpus construction [`width_sweep`](../width_sweep/index.html) uses, so a
//! column holds identical bytes at every width and only the count varies.
//!
//! The corpus is a header row and two data rows. Nothing here reads past the
//! first record, and the second row exists only so that reading one record is
//! not also reaching end of input.
//!
//! # The four cases
//!
//! `construct` builds the parser and stops. `headless` builds it over the same
//! bytes with [`Headers::None`] and reads one record, so the header row is
//! parsed as data and no header work happens at all. `first` builds it and
//! reads one record with the header row consumed as a header. `lookup` builds
//! it and resolves one column by name — the last one, so a scan cannot finish
//! early.
//!
//! Reading them against each other is the point. `first` minus `construct` is
//! what the first record costs including whatever header work it triggers,
//! `first` minus `headless` isolates the header work from the record work that
//! the same bytes cost either way, and `lookup` minus `construct` is what
//! resolving a name costs, which is where a structure built over every header
//! name would appear.
//!
//! A fourth case builds two parsers instead of one, at a single width. It
//! exists only to separate a per-parser construction cost from a one-time
//! initialization that the first parser in a process would pay, which a single
//! measurement cannot distinguish and which turns out to matter here.
//!
//! # Results
//!
//! Callgrind instruction counts. Each case is measured whole, so `first` and
//! `lookup` include the construction that `construct` measures on its own.
//!
//! | Headers | `construct` | `headless` | `first` | `lookup` | `csv` construct | `csv` first |
//! |---------|-------------|------------|---------|----------|-----------------|-------------|
//! |       6 |         692 |      1,121 |   2,173 |    2,867 |         255,568 |     260,599 |
//! |      20 |         692 |      1,862 |   4,215 |    6,161 |         255,568 |     266,705 |
//! |      60 |         692 |      3,452 |   9,345 |   14,671 |         255,568 |     277,140 |
//! |     200 |         692 |      8,742 |  26,915 |   44,566 |         255,568 |     309,809 |
//!
//! # Construction is free here and expensive there
//!
//! `construct` is 692 instructions at every width, because building a
//! `SliceParser` does not look at the header: header resolution is deferred to
//! whatever first needs it. The number is identical at 6 and at 200 columns,
//! which is the clearest possible statement that nothing header-shaped happens
//! in the constructor.
//!
//! The `csv` crate's reader costs 255,568 instructions to construct, also at
//! every width. That is not header work either — it is the same at 6 columns
//! as at 200, and `csv` has not read the header at that point. It is the
//! state machine `csv-core` builds up front.
//!
//! A single measurement could not tell a per-reader cost from a one-time
//! initialization that the first reader in a process happens to pay, and
//! 255,568 is large enough that the distinction matters. So both are measured
//! twice in one benchmark: two `SliceParser`s cost 1,579 against 692 for one,
//! and two `csv` readers cost 511,252 against 255,568 for one. The second
//! reader costs 255,684 — the same as the first. It is a per-reader cost, and
//! a caller that opens many documents pays it on every one of them.
//!
//! Reaching the first record therefore costs 11.5 times less through coseva at
//! 200 columns and 120 times less at 6.
//!
//! # What header setup costs
//!
//! Subtracting `construct` from `first` leaves what the header and the first
//! record cost together:
//!
//! | Headers | coseva | `csv`  |
//! |---------|--------|--------|
//! |       6 |  1,481 |  5,031 |
//! |      20 |  3,523 | 11,137 |
//! |      60 |  8,653 | 21,572 |
//! |     200 | 26,223 | 54,241 |
//!
//! coseva is ahead at every width, and grows at about 127 instructions per
//! column against `csv`'s 254.
//!
//! The `headless` case separates the two halves of that growth. Without it
//! the suite cannot say which part is the header and which is the first
//! record, because every other case reads
//! a record with a header. `headless` parses
//! the identical bytes with [`Headers::None`], so `first` minus `headless` is
//! the header work alone and `headless` minus `construct` is the record work
//! that is paid either way:
//!
//! | Headers | record only | header only |
//! |---------|-------------|-------------|
//! |       6 |         429 |       1,052 |
//! |      20 |       1,170 |       2,353 |
//! |      60 |       2,760 |       5,893 |
//! |     200 |       8,050 |      18,173 |
//!
//! Reading a record grows at about 39 instructions per column, and header
//! setup at about 88, for bytes of the same shape. The gap is `StructCache`:
//! it validates every header name as UTF-8 and copies each one to the heap,
//! 200 allocations for a 200-column file. Running that from
//! `on_headers_changed` for every parser that sets a header — including the
//! great majority that never deserialize anything — costs about 386
//! instructions per column instead, with 40% of `first_200` in the `malloc`
//! family and 15% in `core::str::converts::from_utf8`.
//!
//! So it is deferred to the first Serde call, under the constraint that the
//! deferral cost the Serde path nothing. Two ways of arranging it do not meet
//! that constraint. Testing a readiness flag
//! inside the per-record deserialize entry point costs 1.5% to 2.4% on every
//! Serde benchmark; holding the names in one arena allocation instead of one
//! per column halves the setup win and costs 4% per record, because resolving a
//! name goes from indexing a `Vec` to two bounds-checked offset lookups. What
//! works is folding the readiness test into the header check the Serde path
//! already runs per record, so the second and every later record reaches
//! neither — which makes the Serde suites 1.7% to 3.4% *faster* at 1,000 rows
//! rather than slower.
//!
//! # Resolving a name by string costs more than the record does
//!
//! `lookup` minus `construct` is the header plus one `header_index` call:
//! 2,175 at 6 columns and 43,874 at 200. Subtracting `first` isolates what
//! resolving the name adds over merely having read a record — 694 at 6 columns
//! rising to 17,651 at 200, about 87 instructions per column.
//!
//! That is the structure over the header names being built, and it is the cost
//! `decode_wide` found hiding in its `rows_1` column and made lazy. The shape
//! of this row is the reason it is lazy: a caller that decodes by derive or by
//! Serde never resolves a name this way and never pays it, and this suite is
//! where a change that made it eager again would show up immediately rather
//! than as an unexplained shift in somebody else's `rows_1`.
//!
//! The map keys on each header name's hash and confirms the bytes against the
//! record the engine already owns, rather than copying each name onto the heap
//! to key with. Copying is what a map that outlives the window the header
//! record was parsed from would otherwise need, and it accounts for roughly
//! two thirds of the per-column cost. These
//! two columns are the guard for that choice: doing the collision
//! bookkeeping in a second allocation instead costs 33
//! instructions on `construct`, at every width, which is why it is not done
//! that way.
//!
//! # What this does not measure
//!
//! Steady-state parsing, which every other suite covers. These numbers are
//! paid once per parser, so they matter to a caller that opens many small
//! documents and are close to irrelevant to one that opens a large one — a
//! distinction the per-record suites cannot make and this one cannot make for
//! them.
//!
//! `io` and `push` are left out. Their construction additionally allocates a
//! read buffer whose size is a configured constant rather than a function of
//! the header, so they would add a flat offset to every row without changing
//! how the rows relate to each other.

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

use coseva::SliceParser;
use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(dead_code, reason = "this file builds its own header-swept corpus")]
mod fixture;

use fixture::drop_it;

/// The width of a column name, `cNNN`, fixed so a name costs the same at every
/// width in the sweep.
const NAME_LEN: usize = 4;

/// The width of every field value, so the data rows are a fixed size and only
/// the header count varies.
const VALUE_LEN: usize = 5;

/// The value column `index` carries, at every width.
const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

/// Build one width's corpus and its three measured cases.
macro_rules! width {
    ($module:ident, $columns:literal, $last:literal) => {
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

            /// A header row and two data rows, built at compile time so no case
            /// pays to allocate its input.
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

            static BUF: [u8; HEADER_LEN + ROW_LEN * 2] = corpus();

            pub(super) static INPUT: &[u8] = &BUF;

            /// The name of the last column, resolved by `lookup` so that a scan
            /// over the header cannot finish early.
            pub(super) const LAST: &str = $last;

            /// The number of fields every record carries, asserted so a case
            /// cannot quietly parse something else and still look comparable.
            pub(super) const FIELDS: usize = COLUMNS;
        }
    };
}

width!(w6, 6, "c005");
width!(w20, 20, "c019");
width!(w60, 60, "c059");
width!(w200, 200, "c199");

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::FirstRecord)
}

/// The same configuration with the header policy removed, so the header row is
/// read as data and no header work happens at all.
fn headless_options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

type CsvReader = ::csv::Reader<Cursor<&'static [u8]>>;

fn input(input: &'static [u8]) -> &'static [u8] {
    input
}

/// Build a parser over `input` and return it without reading anything.
fn build(input: &'static [u8]) -> SliceParser<'static, Csv> {
    SliceParser::<Csv>::new(input, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"))
}

/// Build a parser that treats the header row as data.
fn build_headless(input: &'static [u8]) -> SliceParser<'static, Csv> {
    SliceParser::<Csv>::new(input, headless_options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"))
}

/// Build a `csv` reader over `input`, which does not touch the header either
/// until something asks for it.
fn build_csv(input: &'static [u8]) -> CsvReader {
    ::csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(Cursor::new(input))
}

/// Generate the six measured cases for one width.
macro_rules! cases {
    ($construct:ident, $headless:ident, $first:ident, $lookup:ident, $csv_construct:ident, $csv_first:ident, $module:ident) => {
        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $headless(input: &'static [u8]) -> (usize, SliceParser<'static, Csv>) {
            let mut parser = build_headless(input);
            let fields = {
                let mut line = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .expect("the corpus holds three records");
                line.record()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .len()
            };
            assert_eq!(fields, $module::FIELDS, "benchmark read the wrong record");
            (black_box(fields), parser)
        }

        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $construct(input: &'static [u8]) -> SliceParser<'static, Csv> {
            black_box(build(input))
        }

        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $first(input: &'static [u8]) -> (usize, SliceParser<'static, Csv>) {
            let mut parser = build(input);
            let fields = {
                let mut line = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .expect("the corpus holds two records");
                line.record()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .len()
            };
            assert_eq!(fields, $module::FIELDS, "benchmark read the wrong record");
            (black_box(fields), parser)
        }

        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $lookup(input: &'static [u8]) -> (usize, SliceParser<'static, Csv>) {
            let mut parser = build(input);
            let found = parser
                .header_index($module::LAST)
                .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                .expect("the last column is present");
            assert_eq!(
                found,
                $module::FIELDS - 1,
                "benchmark resolved the wrong column"
            );
            (black_box(found), parser)
        }

        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $csv_construct(input: &'static [u8]) -> CsvReader {
            black_box(build_csv(input))
        }

        #[library_benchmark]
        #[bench::w(args = ($module::INPUT), setup = input, teardown = drop_it)]
        fn $csv_first(input: &'static [u8]) -> (usize, CsvReader) {
            let mut reader = build_csv(input);
            let mut record = ::csv::ByteRecord::new();
            let read = reader
                .read_byte_record(&mut record)
                .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
            assert!(read, "the corpus holds two records");
            assert_eq!(
                record.len(),
                $module::FIELDS,
                "benchmark read the wrong record"
            );
            (black_box(record.len()), reader)
        }
    };
}

cases!(
    construct_6,
    headless_6,
    first_6,
    lookup_6,
    csv_construct_6,
    csv_first_6,
    w6
);
cases!(
    construct_20,
    headless_20,
    first_20,
    lookup_20,
    csv_construct_20,
    csv_first_20,
    w20
);
cases!(
    construct_60,
    headless_60,
    first_60,
    lookup_60,
    csv_construct_60,
    csv_first_60,
    w60
);
cases!(
    construct_200,
    headless_200,
    first_200,
    lookup_200,
    csv_construct_200,
    csv_first_200,
    w200
);

// Build two parsers, so that differencing against `construct` separates a
// per-parser cost from any one-time initialization the first one pays. Both
// cases below are width-independent — `construct` measures the same number at
// every width — so one width is enough to answer the question.
#[library_benchmark]
#[bench::once(args = (w6::INPUT), setup = input, teardown = drop_it)]
fn construct_twice(input: &'static [u8]) -> (SliceParser<'static, Csv>, SliceParser<'static, Csv>) {
    (black_box(build(input)), black_box(build(input)))
}

#[library_benchmark]
#[bench::once(args = (w6::INPUT), setup = input, teardown = drop_it)]
fn csv_construct_twice(input: &'static [u8]) -> (CsvReader, CsvReader) {
    (black_box(build_csv(input)), black_box(build_csv(input)))
}

library_benchmark_group!(
    name = startup;
    benchmarks =
        construct_6, headless_6, first_6, lookup_6, csv_construct_6, csv_first_6,
        construct_20, headless_20, first_20, lookup_20, csv_construct_20, csv_first_20,
        construct_60, headless_60, first_60, lookup_60, csv_construct_60, csv_first_60,
        construct_200, headless_200, first_200, lookup_200, csv_construct_200, csv_first_200,
        construct_twice, csv_construct_twice
);

main!(library_benchmark_groups = startup);
