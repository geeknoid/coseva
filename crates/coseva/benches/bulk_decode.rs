//! Decoding a whole source into owned typed structs, the way most callers do.
//!
//! [`decoded_records`] is the bulk entry point: it resolves the target's field
//! mapping against the header once and then yields one independently owned
//! struct per record. Every other decode suite here measures a single record,
//! or `decode_into`'s reuse of one buffer; this one measures the iterator the
//! public API hands out.
//!
//! # The four cases
//!
//! Two widths crossed with two header orders. `narrow` names eight columns,
//! `wide` names 128, so the per-record cost separates the fixed part of
//! reaching a record from the per-field part of decoding it. `ordered` gives
//! the header in the order the struct declares its fields; `reordered` reverses
//! it, so the resolved mapping is a permutation rather than the identity and
//! every field read jumps.
//!
//! The reordered corpus carries each column's value under its own name, so all
//! four cases decode to the same checksum and a case that silently resolved the
//! wrong column would fail rather than post a better number.
//!
//! # Results
//!
//! Callgrind instruction counts over 1,000 records, and the marginal per-record
//! cost from the difference between the 100-row and 1,000-row cases.
//!
//! | Case               |       100 |       1000 | Per record |
//! |--------------------|-----------|------------|------------|
//! | `narrow_ordered`   |   110,738 |  1,048,538 |      1,042 |
//! | `narrow_reordered` |   114,562 |  1,086,562 |      1,080 |
//! | `wide_ordered`     | 1,430,373 | 13,830,573 |     13,778 |
//! | `wide_reordered`   | 1,495,025 | 14,482,925 |     14,431 |
//!
//! # What the numbers say
//!
//! The two widths differ by 120 columns and 12,736 instructions, so a decoded
//! `u64` column costs about 106 instructions and the fixed part of reaching and
//! yielding one owned record is about 190. At eight columns the per-field work
//! is already 80% of the record, so this path is dominated by decoding rather
//! than by the iterator around it.
//!
//! Reordering the header costs 38 instructions per record narrow and 653 wide.
//! That is about 5 per field either way rather than a fixed charge, so what a
//! permuted mapping costs is paid per field read and not once per record.
//!
//! [`decoded_records`]: coseva::SliceParser::decoded_records

#![expect(missing_docs, reason = "benchmark macros generate private modules")]

use std::hint::black_box;

use coseva::SliceParser;
use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use gungraun::prelude::*;

/// The digits every column's value is written with, so a row is fixed width.
const VALUE_LEN: usize = 5;

/// The value carried by the column with index `column`, wherever that column
/// is placed. Keying the value to the name rather than the position is what
/// lets the ordered and reordered corpora share one checksum.
const fn value(column: usize) -> u64 {
    (10_000 + column * 137) as u64
}

/// Declare a decode target of `u64` columns together with its checksum.
macro_rules! target {
    ($name:ident, $($field:ident),+ $(,)?) => {
        #[derive(CsvDecode)]
        struct $name {
            $($field: u64,)+
        }

        impl $name {
            fn total(&self) -> u64 {
                0 $(+ self.$field)+
            }
        }
    };
}

target!(Narrow, c00, c01, c02, c03, c04, c05, c06, c07);

target!(
    Wide, c000, c001, c002, c003, c004, c005, c006, c007, c008, c009, c010, c011, c012, c013, c014,
    c015, c016, c017, c018, c019, c020, c021, c022, c023, c024, c025, c026, c027, c028, c029, c030,
    c031, c032, c033, c034, c035, c036, c037, c038, c039, c040, c041, c042, c043, c044, c045, c046,
    c047, c048, c049, c050, c051, c052, c053, c054, c055, c056, c057, c058, c059, c060, c061, c062,
    c063, c064, c065, c066, c067, c068, c069, c070, c071, c072, c073, c074, c075, c076, c077, c078,
    c079, c080, c081, c082, c083, c084, c085, c086, c087, c088, c089, c090, c091, c092, c093, c094,
    c095, c096, c097, c098, c099, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110,
    c111, c112, c113, c114, c115, c116, c117, c118, c119, c120, c121, c122, c123, c124, c125, c126,
    c127
);

/// Build the corpus and the decode loop for one width.
///
/// `NAME_LEN` is the width of a column name, which differs between the two
/// widths because 128 columns need three digits and eight need two.
macro_rules! width {
    ($module:ident, $target:ident, $columns:literal, $name_len:literal) => {
        mod $module {
            use super::*;

            const COLUMNS: usize = $columns;
            const NAME_LEN: usize = $name_len;
            const HEADER_LEN: usize = COLUMNS * (NAME_LEN + 1);
            const ROW_LEN: usize = COLUMNS * (VALUE_LEN + 1);

            /// Write the decimal name of `column` into `out` at `base`.
            const fn write_name(out: &mut [u8], base: usize, column: usize) {
                out[base] = b'c';
                let mut remaining = column;
                let mut digit = NAME_LEN;
                while digit > 1 {
                    digit -= 1;
                    out[base + digit] = b"0123456789"[remaining % 10];
                    remaining /= 10;
                }
            }

            /// Write the value of `column` into `out` at `base`.
            const fn write_value(out: &mut [u8], base: usize, column: usize) {
                let mut remaining = value(column);
                let mut digit = VALUE_LEN;
                while digit > 0 {
                    digit -= 1;
                    out[base + digit] = b'0' + (remaining % 10) as u8;
                    remaining /= 10;
                }
            }

            /// The column placed at position `at`, given the ordering.
            const fn placed(at: usize, reversed: bool) -> usize {
                if reversed { COLUMNS - 1 - at } else { at }
            }

            const fn header(reversed: bool) -> [u8; HEADER_LEN] {
                let mut out = [0_u8; HEADER_LEN];
                let mut at = 0;
                while at < COLUMNS {
                    let base = at * (NAME_LEN + 1);
                    write_name(&mut out, base, placed(at, reversed));
                    out[base + NAME_LEN] = if at + 1 == COLUMNS { b'\n' } else { b',' };
                    at += 1;
                }
                out
            }

            const fn row(reversed: bool) -> [u8; ROW_LEN] {
                let mut out = [0_u8; ROW_LEN];
                let mut at = 0;
                while at < COLUMNS {
                    let base = at * (VALUE_LEN + 1);
                    write_value(&mut out, base, placed(at, reversed));
                    out[base + VALUE_LEN] = if at + 1 == COLUMNS { b'\n' } else { b',' };
                    at += 1;
                }
                out
            }

            const fn corpus<const N: usize>(reversed: bool) -> [u8; N] {
                let mut out = [0_u8; N];
                let head = header(reversed);
                let body = row(reversed);
                let mut index = 0;
                while index < HEADER_LEN {
                    out[index] = head[index];
                    index += 1;
                }
                let mut offset = 0;
                while HEADER_LEN + offset < N {
                    out[HEADER_LEN + offset] = body[offset % ROW_LEN];
                    offset += 1;
                }
                out
            }

            pub(super) static ORDERED_100: [u8; HEADER_LEN + ROW_LEN * 100] = corpus(false);
            pub(super) static ORDERED_1000: [u8; HEADER_LEN + ROW_LEN * 1000] = corpus(false);
            pub(super) static REORDERED_100: [u8; HEADER_LEN + ROW_LEN * 100] = corpus(true);
            pub(super) static REORDERED_1000: [u8; HEADER_LEN + ROW_LEN * 1000] = corpus(true);

            /// The sum of every column's value, which one record decodes to.
            const fn per_record() -> u64 {
                let mut total = 0;
                let mut column = 0;
                while column < COLUMNS {
                    total += value(column);
                    column += 1;
                }
                total
            }

            pub(super) fn state(input: &'static [u8]) -> (SliceParser<'static, Csv>, usize) {
                let parser = SliceParser::<Csv>::new(input, options())
                    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
                (parser, (input.len() - HEADER_LEN) / ROW_LEN)
            }

            pub(super) fn decode(
                state: (SliceParser<'static, Csv>, usize),
            ) -> (u64, SliceParser<'static, Csv>) {
                let (mut parser, rows) = state;
                let mut total = 0_u64;
                for decoded in parser.decoded_records::<$target>() {
                    let record =
                        decoded.unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(record.total());
                }
                let expected = rows as u64 * per_record();
                assert_eq!(total, expected, "bulk decode read the wrong columns");
                (black_box(total), parser)
            }
        }
    };
}

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::FirstRecord)
}

fn drop_it<T>(value: T) {
    drop(value);
}

width!(narrow, Narrow, 8, 3);
width!(wide, Wide, 128, 4);

type State = (SliceParser<'static, Csv>, usize);

#[library_benchmark]
#[bench::rows_100(args = (narrow::ORDERED_100.as_slice()), setup = narrow::state, teardown = drop_it)]
#[bench::rows_1000(args = (narrow::ORDERED_1000.as_slice()), setup = narrow::state, teardown = drop_it)]
fn narrow_ordered(state: State) -> (u64, SliceParser<'static, Csv>) {
    narrow::decode(state)
}

#[library_benchmark]
#[bench::rows_100(args = (narrow::REORDERED_100.as_slice()), setup = narrow::state, teardown = drop_it)]
#[bench::rows_1000(args = (narrow::REORDERED_1000.as_slice()), setup = narrow::state, teardown = drop_it)]
fn narrow_reordered(state: State) -> (u64, SliceParser<'static, Csv>) {
    narrow::decode(state)
}

#[library_benchmark]
#[bench::rows_100(args = (wide::ORDERED_100.as_slice()), setup = wide::state, teardown = drop_it)]
#[bench::rows_1000(args = (wide::ORDERED_1000.as_slice()), setup = wide::state, teardown = drop_it)]
fn wide_ordered(state: State) -> (u64, SliceParser<'static, Csv>) {
    wide::decode(state)
}

#[library_benchmark]
#[bench::rows_100(args = (wide::REORDERED_100.as_slice()), setup = wide::state, teardown = drop_it)]
#[bench::rows_1000(args = (wide::REORDERED_1000.as_slice()), setup = wide::state, teardown = drop_it)]
fn wide_reordered(state: State) -> (u64, SliceParser<'static, Csv>) {
    wide::decode(state)
}

library_benchmark_group!(
    name = bulk;
    benchmarks = narrow_ordered, narrow_reordered, wide_ordered, wide_reordered
);

main!(library_benchmark_groups = bulk);
