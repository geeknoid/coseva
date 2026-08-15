//! Separates structural search from record materialization as row width grows.
//!
//! Every case processes the same 1,000 plain rows at widths 6, 20, 60, 128,
//! and 200. `scan_only` counts delimiters and record endings without building
//! fields. The other cases build a borrowed record, copy into a pre-grown
//! `ByteRecord`, or decode two positional integers after the full record has
//! been materialized. Every result is checked so a faster but incomplete scan
//! cannot become a benchmark result.
//!
//! Callgrind Ir for 1,000 rows:
//!
//! | Width | Scan only | Borrowed | Owned | Typed | Borrowed minus scan/row |
//! |------:|----------:|---------:|------:|------:|------------------------:|
//! | 6     | 60,788    | 385,138  | 614,482 | 575,138 | 324.4 |
//! | 20    | 202,538   | 894,602  | 1,181,684 | 1,084,602 | 692.1 |
//! | 60    | 607,538   | 1,604,844 | 3,163,409 | 1,794,844 | 997.3 |
//! | 128   | 1,296,038 | 3,171,743 | 6,270,778 | 3,361,743 | 1,875.7 |
//! | 200   | 2,025,038 | 4,892,188 | 9,787,117 | 5,082,188 | 2,867.2 |
//!
//! The scan remains 10.1 instructions per column per row. On the wide path,
//! borrowed materialization now adds about 13.4, down from 26.8 before delimiter
//! masks produced spans directly; widths 60–200 improve by 32–35%.

#![expect(missing_docs, reason = "benchmark macros generate private modules")]
#![expect(
    long_running_const_eval,
    reason = "the benchmark corpora are deliberately compile-time constants"
)]
#![expect(
    clippy::large_stack_arrays,
    clippy::large_stack_frames,
    reason = "const evaluation builds static benchmark corpora, not runtime stack arrays"
)]

use std::hint::black_box;

use coseva::benchmark::scan_selected;
use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::{ByteRecord, SliceParser};
use gungraun::prelude::*;
use gungraun::{Callgrind, EventKind};

const ROWS: usize = 1_000;
const VALUE_LEN: usize = 5;

const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

#[derive(CsvDecode)]
struct Typed(u64, u64);

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn check_scan(total: usize, columns: usize) -> usize {
    assert_eq!(total, ROWS * columns, "scanner missed structural bytes");
    total
}

fn check_record(total: u64, columns: usize) -> u64 {
    let per_row = columns as u64 + (VALUE_LEN * 2) as u64;
    assert_eq!(total, ROWS as u64 * per_row, "wrong record shape");
    total
}

fn check_typed(total: u64) -> u64 {
    let expected = ROWS as u64 * (value(0) + value(1));
    assert_eq!(total, expected, "typed decode read the wrong fields");
    total
}

fn drop_it<T>(value: T) {
    drop(value);
}

macro_rules! width {
    ($module:ident, $columns:literal) => {
        mod $module {
            use super::*;

            const COLUMNS: usize = $columns;
            const ROW_LEN: usize = COLUMNS * (VALUE_LEN + 1);

            const fn row() -> [u8; ROW_LEN] {
                let mut out = [0_u8; ROW_LEN];
                let mut column = 0;
                while column < COLUMNS {
                    let base = column * (VALUE_LEN + 1);
                    let mut remaining = value(column);
                    let mut digit = VALUE_LEN;
                    while digit > 0 {
                        digit -= 1;
                        out[base + digit] = b'0' + (remaining % 10) as u8;
                        remaining /= 10;
                    }
                    out[base + VALUE_LEN] = if column + 1 == COLUMNS { b'\n' } else { b',' };
                    column += 1;
                }
                out
            }

            const ROW: [u8; ROW_LEN] = row();

            const fn corpus() -> [u8; ROW_LEN * ROWS] {
                let mut out = [0_u8; ROW_LEN * ROWS];
                let mut index = 0;
                while index < out.len() {
                    out[index] = ROW[index % ROW_LEN];
                    index += 1;
                }
                out
            }

            static CORPUS: [u8; ROW_LEN * ROWS] = corpus();
            pub(super) static INPUT: &[u8] = &CORPUS;

            pub(super) fn borrowed_state() -> SliceParser<'static, Csv> {
                SliceParser::<Csv>::new(INPUT, options())
                    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"))
            }

            pub(super) fn owned_state() -> (SliceParser<'static, Csv>, ByteRecord) {
                (
                    borrowed_state(),
                    ByteRecord::with_capacity(COLUMNS, COLUMNS * VALUE_LEN),
                )
            }

            pub(super) fn scan() -> usize {
                black_box(check_scan(scan_selected(INPUT, b',', b'"', b'\n'), COLUMNS))
            }

            pub(super) fn borrowed(
                mut parser: SliceParser<'static, Csv>,
            ) -> (u64, SliceParser<'static, Csv>) {
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    let record = line
                        .record()
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(
                        record.len() as u64
                            + record.get(0).map_or(0, <[u8]>::len) as u64
                            + record.get(COLUMNS - 1).map_or(0, <[u8]>::len) as u64,
                    );
                }
                (black_box(check_record(total, COLUMNS)), parser)
            }

            pub(super) fn owned(
                state: (SliceParser<'static, Csv>, ByteRecord),
            ) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
                let (mut parser, mut record) = state;
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    line.read_byte_record_into(&mut record)
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(
                        record.len() as u64
                            + record.get(0).map_or(0, <[u8]>::len) as u64
                            + record.get(COLUMNS - 1).map_or(0, <[u8]>::len) as u64,
                    );
                }
                (black_box(check_record(total, COLUMNS)), parser, record)
            }

            pub(super) fn typed(
                mut parser: SliceParser<'static, Csv>,
            ) -> (u64, SliceParser<'static, Csv>) {
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    let row: Typed = line
                        .decoded()
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add(row.0 + row.1);
                }
                (black_box(check_typed(total)), parser)
            }
        }
    };
}

width!(w6, 6);
width!(w20, 20);
width!(w60, 60);
width!(w128, 128);
width!(w200, 200);

macro_rules! cases {
    (
        $scan:ident,
        $borrowed:ident,
        $owned:ident,
        $typed:ident,
        $module:ident,
        $scan_limit:literal,
        $borrowed_limit:literal,
        $typed_limit:literal
    ) => {
        #[library_benchmark(
                            config = LibraryBenchmarkConfig::default().tool_override(
                                Callgrind::default().hard_limits([(EventKind::Ir, $scan_limit)])
                            )
                        )]
        fn $scan() -> usize {
            $module::scan()
        }

        #[library_benchmark(
            config = LibraryBenchmarkConfig::default().tool_override(
                Callgrind::default().hard_limits([(EventKind::Ir, $borrowed_limit)])
            ),
            setup = $module::borrowed_state,
            teardown = drop_it
        )]
        fn $borrowed(parser: SliceParser<'static, Csv>) -> (u64, SliceParser<'static, Csv>) {
            $module::borrowed(parser)
        }

        #[library_benchmark(setup = $module::owned_state, teardown = drop_it)]
        fn $owned(
            state: (SliceParser<'static, Csv>, ByteRecord),
        ) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
            $module::owned(state)
        }

        #[library_benchmark(
            config = LibraryBenchmarkConfig::default().tool_override(
                Callgrind::default().hard_limits([(EventKind::Ir, $typed_limit)])
            ),
            setup = $module::borrowed_state,
            teardown = drop_it
        )]
        fn $typed(parser: SliceParser<'static, Csv>) -> (u64, SliceParser<'static, Csv>) {
            $module::typed(parser)
        }
    };
}

// Hard limits are the measured Callgrind Ir baselines plus 2%.
cases!(
    scan_6, borrowed_6, owned_6, typed_6, w6, 62_004, 389_144, 582_944
);
cases!(
    scan_20,
    borrowed_20,
    owned_20,
    typed_20,
    w20,
    206_589,
    915_043,
    1_108_843
);
cases!(
    scan_60,
    borrowed_60,
    owned_60,
    typed_60,
    w60,
    619_689,
    1_636_125,
    1_829_925
);
cases!(
    scan_128,
    borrowed_128,
    owned_128,
    typed_128,
    w128,
    1_321_959,
    3_233_452,
    3_427_252
);
cases!(
    scan_200,
    borrowed_200,
    owned_200,
    typed_200,
    w200,
    2_065_539,
    4_987_327,
    5_181_128
);

library_benchmark_group!(
    name = materialization;
    benchmarks =
        scan_6,
        borrowed_6,
        owned_6,
        typed_6,
        scan_20,
        borrowed_20,
        owned_20,
        typed_20,
        scan_60,
        borrowed_60,
        owned_60,
        typed_60,
        scan_128,
        borrowed_128,
        owned_128,
        typed_128,
        scan_200,
        borrowed_200,
        owned_200,
        typed_200
);

main!(library_benchmark_groups = materialization);
