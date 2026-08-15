//! Steady-state typed decoding into reusable heap-backed fields.
//!
//! Setup consumes one warm-up row after pre-growing the `String` and `Vec<u8>`
//! fields. The measured region then decodes exactly 1,000 rows, at widths 6
//! and 200, without replacing either allocation.
//!
//! The committed Callgrind Ir baselines are 566,660 narrow and 7,735,779 wide;
//! each case carries a hard limit at 102% of that measured value.

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
#![expect(
    clippy::must_use_candidate,
    reason = "Gungraun benchmark entry points are invoked by the generated harness"
)]

use std::hint::black_box;

use coseva::SliceParser;
use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use gungraun::prelude::*;
use gungraun::{Callgrind, EventKind};

const ROWS: usize = 1_000;
const VALUE_LEN: usize = 5;

const fn value(index: usize) -> u64 {
    (10_000 + index * 137) as u64
}

#[derive(CsvDecode)]
struct Reused(String, Vec<u8>);

type State = (SliceParser<'static, Csv>, Reused);

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn drop_it<T>(value: T) {
    drop(value);
}

fn check(total: u64) -> u64 {
    let expected = ROWS as u64 * (VALUE_LEN * 2) as u64;
    assert_eq!(total, expected, "typed decode read the wrong fields");
    total
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

            const fn corpus() -> [u8; ROW_LEN * (ROWS + 1)] {
                let mut out = [0_u8; ROW_LEN * (ROWS + 1)];
                let mut index = 0;
                while index < out.len() {
                    out[index] = ROW[index % ROW_LEN];
                    index += 1;
                }
                out
            }

            static CORPUS: [u8; ROW_LEN * (ROWS + 1)] = corpus();

            pub(super) fn state() -> State {
                let mut parser = SliceParser::<Csv>::new(&CORPUS, options())
                    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
                let mut output = Reused(
                    String::with_capacity(VALUE_LEN),
                    Vec::with_capacity(VALUE_LEN),
                );
                let mut warmup = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                    .expect("warm-up row");
                warmup
                    .decode_into(&mut output)
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                (parser, output)
            }

            pub(super) fn decode(state: State) -> (u64, SliceParser<'static, Csv>, Reused) {
                let (mut parser, mut output) = state;
                let mut total = 0_u64;
                while let Some(mut line) = parser
                    .next_line()
                    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
                {
                    line.decode_into(&mut output)
                        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
                    total = total.wrapping_add((output.0.len() + output.1.len()) as u64);
                }
                (black_box(check(total)), parser, output)
            }
        }
    };
}

width!(narrow, 6);
width!(wide, 200);

// Hard limits are the measured Callgrind Ir baselines plus 2%.
#[library_benchmark(
    config = LibraryBenchmarkConfig::default().tool_override(
        Callgrind::default().hard_limits([(EventKind::Ir, 577_994)])
    ),
    setup = narrow::state,
    teardown = drop_it
)]
fn narrow_decode_into(state: State) -> (u64, SliceParser<'static, Csv>, Reused) {
    narrow::decode(state)
}

#[library_benchmark(
    config = LibraryBenchmarkConfig::default().tool_override(
        Callgrind::default().hard_limits([(EventKind::Ir, 7_890_495)])
    ),
    setup = wide::state,
    teardown = drop_it
)]
fn wide_decode_into(state: State) -> (u64, SliceParser<'static, Csv>, Reused) {
    wide::decode(state)
}

library_benchmark_group!(
    name = decode_into;
    benchmarks = narrow_decode_into, wide_decode_into
);

main!(library_benchmark_groups = decode_into);
