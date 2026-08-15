//! Decoding two columns out of 6, 20, 60 and 200, to measure how cost grows
//! with width.
//!
//! [`decode`](../decode/index.html) measures a six-column record and
//! [`decode_wide`](../decode_wide/index.html) measures a hundred-column one,
//! and `decode_wide` reads the two together as a straight line to estimate a
//! per-column cost. Its own documentation says why that is an estimate rather
//! than a measurement: the two corpora do not hold the same field content, so
//! part of the gap between them is field width and type rather than column
//! count, and two points cannot show curvature at all.
//!
//! This suite exists to replace that estimate. It sweeps four widths whose
//! field content is identical column for column, so the only thing that varies
//! is how many columns there are.
//!
//! # The corpus
//!
//! Column `i` is named `cNNN` — always four characters, so a name costs the
//! same at every width — and holds `10000 + i * 137`, always five digits. A
//! column therefore holds exactly the same bytes at width 6 as it does at
//! width 200, and a record is `width * 6` bytes. Fields are unquoted ASCII
//! digits with no escapes, which flatters both crates equally and keeps the
//! measurement about column count rather than about quote handling.
//!
//! # What is decoded
//!
//! Two columns at every width: `c000` and the last column. Two rather than
//! five because five will not fit in a six-column record, and first-and-last
//! rather than any interior pair because it forces the parse to cross the
//! whole record at every width. If the target columns sat in a prefix, a front
//! end that stopped once it had them would be measured on a shorter record at
//! the wider settings, and the sweep would be measuring two different things.
//!
//! Holding the target at two columns is what makes the sweep answer the
//! question it is for. The columns that are named cost the same at every
//! width, so everything the sweep shows moving is the cost of the columns
//! nobody asked for.
//!
//! # Reading the numbers
//!
//! Per record is `(rows_1000 - rows_100) / 900`, which cancels the fixed
//! startup — header resolution, allocation, and the `csv` crate's separate
//! header pass — the same way every other suite here does. Per column is the
//! per-record figure divided by the width.
//!
//! # Results
//!
//! Callgrind instruction counts, `slice` against `csv`. Per record is
//! `(rows_1000 - rows_100) / 900`; per column is that divided by the width.
//!
//! | Width | `slice` per record | per column | `csv` per record | per column | `slice` vs `csv` |
//! |-------|--------------------|------------|------------------|------------|------------------|
//! |     6 |                566 |       94.3 |            1,546 |      257.6 |             -63% |
//! |    20 |              1,083 |       54.2 |            3,950 |      197.5 |             -73% |
//! |    60 |              2,563 |       42.7 |           10,817 |      180.3 |             -76% |
//! |   200 |              7,744 |       38.7 |           34,685 |      173.4 |             -78% |
//!
//! # Both sides are linear, and neither is super-linear
//!
//! This is the question the suite was written to settle, and the answer is
//! that nothing here bends.
//!
//! A least-squares line through the four `slice` points is 343 instructions
//! per record plus 37.0 per column. The measured points sit on that line to
//! within 0.4 instructions — a relative error of 0.07% at the worst point,
//! across a thirty-three-fold range of widths. `csv` is linear too, on a much
//! steeper line: 539 per record plus 170.8 per column, with its points within
//! 1.2% of it. Neither fit shows the upward curvature a super-linear cost
//! would produce, and the `slice` fit is close enough to exact that there is
//! no room for one to hide.
//!
//! The per-column column falls as width grows — 94.3 down to 38.7 for `slice`
//! — and that is the fixed per-record cost being spread over more columns, not
//! a per-column cost that improves. It is worth stating plainly because the
//! shape of that column invites the opposite reading: subtract the fitted
//! fixed cost first and what remains is flat at 37.0 for every width.
//!
//! The margin over `csv` widens from 63% to 78% for the same reason and no
//! other. Both costs are linear, `csv`'s slope is 4.6 times steeper, and so the
//! ratio between them tends toward that slope ratio as the fixed costs stop
//! mattering. It is not evidence that coseva scales better in any asymptotic
//! sense; both are O(columns).
//!
//! # This confirms the estimate it replaces
//!
//! `decode_wide` estimated 39 instructions per column for `slice` against 178
//! for `csv`, on fixed costs of about 420 and 650, by reading two benchmarks
//! whose corpora differed in more than width. The measured figures are 37.0
//! and 170.8, on fixed costs of 343 and 539.
//!
//! The estimate was a little high on all four numbers, which is what one would
//! expect from two points whose wider corpus also carried different field
//! content. But it had the slopes right to within 6%, and the conclusions
//! `decode_wide` drew from it — that the per-column cost differs far more than
//! the per-record cost, and that this is why the margin grows with width — are
//! exactly what the sweep shows. The estimate can now be retired in favour of
//! this, rather than merely believed.
//!
//! # What this does not measure
//!
//! Only the borrowed decode path through `SliceParser`, and only plain
//! unquoted fields. `push` and `io` are left out because the window and
//! buffer-refill bookkeeping they add is per record rather than per column, so
//! they would shift every row of the table by roughly the same amount without
//! changing its shape — which is the only thing this suite is asking about.
//! Their absolute standing against `csv` is what `decode_wide` is for.
//!
//! Instruction counts are not wall time, and a per-column cost that is flat in
//! instructions could still vary in cycles if wider records changed cache
//! behaviour. The `L1` and `RAM` figures gungraun prints alongside these are
//! where that would show.

#![expect(
    missing_docs,
    reason = "the benchmark macros generate private modules of their own"
)]
#![expect(
    clippy::cast_possible_truncation,
    reason = "the corpus builder's digits are one decimal place by construction"
)]

use std::hint::black_box;

use coseva::SliceParser;
use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this file builds its own width-swept corpus and checksum"
)]
mod fixture;

use fixture::drop_it;

/// The width of a column name, `cNNN`, fixed so a name costs the same at every
/// width in the sweep.
const NAME_LEN: usize = 4;

/// The width of every field value, so a record is a fixed size and the corpus
/// can be differenced without a length distribution moving alongside the width.
const VALUE_LEN: usize = 5;

/// The value column `index` carries, at every width.
///
/// Five digits for every index the sweep reaches, so the bytes of column `i`
/// are identical whether the record has six columns or two hundred.
const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

/// Build one width's corpus, target struct, and pair of benchmark cases.
///
/// A macro rather than const generics because each width needs a decode target
/// naming its own last column, and a field name cannot be computed from a const
/// parameter.
macro_rules! width {
    ($module:ident, $columns:literal, $last:ident, $last_index:literal) => {
        mod $module {
            use super::*;

            pub(super) const COLUMNS: usize = $columns;
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
            #[derive(CsvDecode)]
            struct Target<'input> {
                c000: &'input str,
                $last: u64,
            }

            #[derive(serde::Deserialize)]
            struct CsvTarget<'input> {
                c000: &'input str,
                $last: u64,
            }

            /// The checksum one record contributes: the length of the borrowed
            /// first column plus the parsed last one.
            const PER_RECORD: u64 = VALUE_LEN as u64 + value($last_index);

            fn rows_in(input: &[u8]) -> u64 {
                ((input.len() - HEADER_LEN) / ROW_LEN) as u64
            }

            /// Assert the case decoded the intended two columns of every
            /// record, so a mapping that resolved to the wrong offsets cannot
            /// still look comparable.
            fn check(total: u64, input: &[u8]) -> u64 {
                let expected = rows_in(input) * PER_RECORD;
                assert_eq!(total, expected, "benchmark decoded the wrong fields");
                total
            }

            pub(super) fn slice_state(input: &'static [u8]) -> SliceState {
                let parser = SliceParser::<Csv>::new(input, options())
                    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
                (parser, input)
            }

            pub(super) fn csv_state(input: &'static [u8]) -> CsvState {
                let mut reader = ::csv::ReaderBuilder::new()
                    .has_headers(true)
                    .from_reader(std::io::Cursor::new(input));
                let headers = reader
                    .byte_headers()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .clone();
                (reader, ::csv::ByteRecord::new(), headers, input)
            }

            pub(super) fn slice(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
                let (mut parser, input) = state;
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    let row: Target<'_> = line
                        .decoded()
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(row.c000.len() as u64 + row.$last);
                }
                (black_box(check(total, input)), parser)
            }

            pub(super) fn csv(state: CsvState) -> (u64, CsvReader, ::csv::ByteRecord) {
                let (mut reader, mut record, headers, input) = state;
                let mut total = 0_u64;
                while reader
                    .read_byte_record(&mut record)
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    let row: CsvTarget<'_> = record
                        .deserialize(Some(&headers))
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(row.c000.len() as u64 + row.$last);
                }
                (black_box(check(total, input)), reader, record)
            }
        }
    };
}

type SliceState = (SliceParser<'static, Csv>, &'static [u8]);
type CsvReader = ::csv::Reader<std::io::Cursor<&'static [u8]>>;
type CsvState = (
    CsvReader,
    ::csv::ByteRecord,
    ::csv::ByteRecord,
    &'static [u8],
);

/// The default limits already admit a two-hundred-column record, and
/// `SliceParser` never refills, so neither a limit nor a buffer size needs
/// setting here — only header resolution does.
fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::FirstRecord)
}

width!(w6, 6, c005, 5);
width!(w20, 20, c019, 19);
width!(w60, 60, c059, 59);
width!(w200, 200, c199, 199);

/// Generate the two measured cases for one width.
///
/// Only `rows_100` and `rows_1000` are measured, because per record is their
/// difference over 900 and no other row count feeds the sweep.
macro_rules! cases {
    ($slice_fn:ident, $csv_fn:ident, $module:ident) => {
        #[library_benchmark]
        #[bench::rows_100(args = ($module::ROWS_100), setup = $module::slice_state, teardown = drop_it)]
        #[bench::rows_1000(args = ($module::ROWS_1000), setup = $module::slice_state, teardown = drop_it)]
        fn $slice_fn(state: SliceState) -> (u64, SliceParser<'static, Csv>) {
            $module::slice(state)
        }

        #[library_benchmark]
        #[bench::rows_100(args = ($module::ROWS_100), setup = $module::csv_state, teardown = drop_it)]
        #[bench::rows_1000(args = ($module::ROWS_1000), setup = $module::csv_state, teardown = drop_it)]
        fn $csv_fn(state: CsvState) -> (u64, CsvReader, ::csv::ByteRecord) {
            $module::csv(state)
        }
    };
}

cases!(slice_6, csv_6, w6);
cases!(slice_20, csv_20, w20);
cases!(slice_60, csv_60, w60);
cases!(slice_200, csv_200, w200);

library_benchmark_group!(
    name = width;
    benchmarks = slice_6, csv_6, slice_20, csv_20, slice_60, csv_60, slice_200, csv_200
);

main!(library_benchmark_groups = width);
