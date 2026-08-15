//! What the four dialect options beyond plain CSV actually cost.
//!
//! Four supported configurations are the ones a naive implementation routes
//! away from the SIMD kernel and onto the spans-based general machine: a
//! `RecordEnding::CrLf` dialect,
//! `Escape::Mysql`, any `Nulls` policy other than `None`, and a `Whitespace`
//! policy that exempts quoted fields. This suite is what puts a number on the
//! tradeoff each one makes.
//!
//! Each is handled on the vectorized path — the `CrLf` and NULL policies as
//! a pass over the finished record, the quote-exempting trim by recognizing
//! that the kernel never produces a quoted field, and `Escape::Mysql` by
//! declining only those records that actually contain a backslash. The suite
//! remains as the standing regression guard for that work.
//!
//! - `src/engine.rs` — `needs_general_parsing` and `needs_record_pass`
//!
//! # What is varied, and what is not
//!
//! Every case reads the same six-field records through `SliceParser` into the
//! same `ByteRecord`, and asserts the same checksum. Only the configuration
//! changes. `slice` alone is measured because the question is what the parser
//! core costs, not what a front end adds, and every front end shares that core.
//!
//! The original rows contain no `\N`, empty field, padding, or backslash. They
//! isolate the tax of enabling each option when a record does not use it. The
//! `triggered` group complements them with one asserted 1,000-row corpus per
//! behavior: `MySQL` escapes, both NULL policies, trimming, CRLF, comments, and a
//! multi-byte delimiter. Keeping the groups separate prevents feature work
//! from being mistaken for option dispatch.
//!
//! Every case uses the run-time-format parser, the one `with_options` builds,
//! rather than the `SliceParser<Csv>` the other suites use. That holds format
//! dispatch constant across all six, so the only thing varying is the dialect.
//! It also means the `specialized` row here is not comparable to the tables in
//! the other suites, which specialize the kernel on the format type.
//!
//! `crlf` is the one case that cannot share the corpus, since a
//! `RecordEnding::CrLf` dialect will not accept a bare newline as a terminator.
//! It reads a `\r\n` corpus whose rows are therefore one byte wider, and its
//! number includes scanning that byte.
//!
//! The `general_` prefix on the case names marks the cases that ask for a
//! non-default dialect option.
//!
//! # Results
//!
//! Callgrind instruction counts. "Per record" is the marginal cost from the
//! difference between 100 and 1000 rows; every case here is linear to within
//! 0.1%.
//!
//! | Case                     | 1     | 10     | 100     | 1000      | Per record | vs specialized | vs `csv` |
//! |--------------------------|-------|--------|---------|-----------|------------|----------------|----------|
//! | `specialized_static`     | 1,693 |  9,567 |  87,578 |   867,807 |        867 | 0.98x          | 0.81x    |
//! | `specialized`            | 5,825 | 13,846 |  93,199 |   886,912 |        882 | 1.00x          | 0.83x    |
//! | `csv`                    | 1,860 | 11,456 | 107,198 | 1,065,661 |      1,065 | 1.21x          | 1.00x    |
//! | `general_nulls_mysql`    | 5,701 | 17,356 | 132,972 | 1,289,767 |      1,285 | 1.46x          | 1.21x    |
//! | `general_nulls_postgres` | 5,721 | 17,430 | 133,586 | 1,295,781 |      1,291 | 1.46x          | 1.21x    |
//! | `general_mysql_escape`   | 6,125 | 19,635 | 153,852 | 1,496,669 |      1,492 | 1.69x          | 1.40x    |
//! | `general_crlf`           | 6,053 | 18,973 | 148,923 | 1,448,160 |      1,444 | 1.64x          | 1.36x    |
//! | `general_trim_unquoted`  | 5,919 | 18,727 | 145,924 | 1,418,541 |      1,414 | 1.60x          | 1.33x    |
//!
//! The `vs specialized` column for `general_crlf`, `general_mysql_escape` and
//! the trim fallback is gated: `scripts/perf_gate.py` fails if any of them
//! leaves its band, because the public API docs quote those multiples and no
//! single-row 2% band can protect a ratio.
//!
//! # Specialized options cost 1.5-1.7x
//!
//! All four sit between 1.46x and 1.69x the specialized parser on
//! byte-identical content that never once exercises the option it asked for,
//! and all four are above `csv`. The spread is the honest cost of the option
//! itself: a per-record validation pass for `CrLf` and NULLs, a per-record
//! backslash search for `MySQL` escapes, and per-field trimming for the trim
//! policy.
//!
//! `general_nulls_postgres` and `general_nulls_mysql` differ by 6 instructions
//! per record, which is the expected shape: both take the same record pass and
//! differ only in a comparison that never matches here.
//!
//! # Asking for the format at run time costs 2%
//!
//! `specialized` is the same parser as `specialized_static` with the format
//! carried in a value rather than a type: 882 instructions per record against
//! 867. The format is classified once at construction and the default packed
//! parser is shared, leaving a 1.7% dynamic-dispatch premium. The multiples
//! above are all measured against that dynamic parser; a dialect option on the
//! static parser is a different question this suite does not ask.
//!
//! # A caution about reading this table against the others
//!
//! `specialized_static` here is the same work as `quoted`'s `slice` case on its
//! `plain` corpus — same parser type, same bytes, same record shape, same
//! summing loop — yet the two suites do not produce interchangeable counts.
//!
//! Each benchmark file is a separate binary, and the optimizer's inlining
//! decisions inside a measured loop turn out to depend on the rest of that
//! binary. That was confirmed directly: in a throwaway suite, adding an
//! unrelated benchmark function to a file moved an unchanged case by 23%, and
//! deleting it again restored the original count exactly. Record capacity,
//! shared functions and input alignment were each tested and ruled out first.
//! See `fixture.rs` for the full account.
//!
//! The consequence matters more than the cause: **numbers from different files
//! in this directory are not comparable to each other.** That is why the
//! `csv` and `specialized_static` reference rows are measured here rather than
//! quoted from `quoted` or `byte_record`, and it is why every claim above is
//! between rows of the table above.
//!
//! # What this does not measure
//!
//! The options on content that actually triggers them — a corpus of `\N`
//! fields under `Nulls::Mysql`, of backslash escapes under `Escape::Mysql`, or
//! of padded fields under a trimming policy. Those measure the feature; this
//! measures the tax. `MySQL` escaping in particular now has two prices, and
//! only the cheaper one is in the table above.
//!
//! Quoted input, which [`quoted`](../quoted/index.html) covers, and which is
//! where these dialects may well behave differently again.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{
    Escape, FormatOptions, Headers, Nulls, ParseOptions, Quoting, RecordEnding, Whitespace,
};
use coseva::format::{Csv, Dynamic};
use coseva::{ByteRecord, SliceParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(dead_code, reason = "this file needs a CRLF corpus of its own")]
mod fixture;

use fixture::{
    BUFFER, FIELD_BYTES, FIELDS, ROW_LEN, ROWS_1, ROWS_10, ROWS_100, ROWS_1000, drop_it,
};

static EXPECTED_FIELDS: [&[u8]; FIELDS] = [
    b"Boston",
    b"Massachusetts",
    b"4500000",
    b"42.3601",
    b"-71.0589",
    b"true",
];

/// The shared row, terminated by `\r\n` instead of `\n`.
static CRLF_ROW: &[u8] = b"Boston,Massachusetts,4500000,42.3601,-71.0589,true\r\n";

const CRLF_LEN: usize = ROW_LEN + 1;

const fn crlf_corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        out[index] = CRLF_ROW[index % CRLF_LEN];
        index += 1;
    }
    out
}

static CRLF_1: [u8; CRLF_LEN] = crlf_corpus();
static CRLF_10: [u8; CRLF_LEN * 10] = crlf_corpus();
static CRLF_100: [u8; CRLF_LEN * 100] = crlf_corpus();
static CRLF_1000: [u8; CRLF_LEN * 1000] = crlf_corpus();

/// A corpus, the number of records in it, and the format to read it under.
type Input = (&'static [u8], usize, FormatOptions);

fn baseline(bytes: &'static [u8], rows: usize) -> Input {
    (bytes, rows, FormatOptions::CSV)
}

fn crlf(bytes: &'static [u8], rows: usize) -> Input {
    (
        bytes,
        rows,
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
    )
}

fn mysql_escape(bytes: &'static [u8], rows: usize) -> Input {
    (
        bytes,
        rows,
        FormatOptions::CSV
            .escape(Escape::Mysql)
            .quoting(Quoting::Never),
    )
}

fn nulls_postgres(bytes: &'static [u8], rows: usize) -> Input {
    (bytes, rows, FormatOptions::CSV.nulls(Nulls::PostgresCsv))
}

fn nulls_mysql(bytes: &'static [u8], rows: usize) -> Input {
    (bytes, rows, FormatOptions::CSV.nulls(Nulls::Mysql))
}

fn trim_unquoted(bytes: &'static [u8], rows: usize) -> Input {
    (
        bytes,
        rows,
        FormatOptions::CSV.trim(Whitespace::FIELDS.unquoted_only()),
    )
}

type State = (SliceParser<'static, Dynamic>, ByteRecord, usize);

fn assert_fields(record: &ByteRecord) {
    assert_eq!(record.len(), EXPECTED_FIELDS.len(), "wrong field count");
    for (index, expected) in EXPECTED_FIELDS.iter().enumerate() {
        assert_eq!(record.get(index), Some(*expected), "wrong field {index}");
    }
}

fn verify_dynamic_input(bytes: &'static [u8], rows: usize, format: FormatOptions) {
    assert_eq!(bytes.len() % rows, 0, "corpus rows must have equal width");
    let row_width = bytes.len() / rows;
    let row = &bytes[..row_width];
    let mut parser = SliceParser::<Dynamic>::with_options(
        row,
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    let mut record = ByteRecord::with_capacity(FIELDS, CRLF_LEN);
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark oracle failed: {error}"))
        .expect("benchmark oracle stopped before its row");
    line.read_byte_record_into(&mut record)
        .unwrap_or_else(|error| panic!("benchmark oracle failed: {error}"));
    assert_fields(&record);
    assert_eq!(record.index(), 0, "wrong record index");
    assert_eq!(record.byte_range(), 0..row_width, "wrong record boundary");
    assert!(
        parser
            .next_line()
            .unwrap_or_else(|error| panic!("benchmark oracle failed at EOF: {error}"))
            .is_none(),
        "benchmark oracle parsed extra records"
    );
}

fn verify_static_input(bytes: &'static [u8], rows: usize) {
    assert_eq!(bytes.len() % rows, 0, "corpus rows must have equal width");
    let row_width = bytes.len() / rows;
    let mut parser = SliceParser::<Csv>::new(
        &bytes[..row_width],
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    let mut record = ByteRecord::with_capacity(FIELDS, CRLF_LEN);
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark oracle failed: {error}"))
        .expect("benchmark oracle stopped before its row");
    line.read_byte_record_into(&mut record)
        .unwrap_or_else(|error| panic!("benchmark oracle failed: {error}"));
    assert_fields(&record);
    assert_eq!(record.index(), 0, "wrong record index");
    assert_eq!(record.byte_range(), 0..row_width, "wrong record boundary");
    assert!(
        parser
            .next_line()
            .unwrap_or_else(|error| panic!("benchmark oracle failed at EOF: {error}"))
            .is_none(),
        "benchmark oracle parsed extra records"
    );
}

fn state(input: Input) -> State {
    let (bytes, rows, format) = input;
    verify_dynamic_input(bytes, rows, format);
    let parser = SliceParser::<Dynamic>::with_options(
        bytes,
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(FIELDS, CRLF_LEN), rows)
}

/// Assert the case saw every field of every record.
///
/// Every configuration yields identical fields over these corpora, so one
/// expectation covers them all and a case whose dialect changed what it parsed
/// would fail rather than report a different number.
fn check(total: u64, rows: usize) -> u64 {
    let expected = rows as u64 * FIELD_BYTES;
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    total
}

type StaticState = (SliceParser<'static, Csv>, ByteRecord, usize);
type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    usize,
);

fn static_state(bytes: &'static [u8], rows: usize) -> StaticState {
    verify_static_input(bytes, rows);
    let parser = SliceParser::<Csv>::new(
        bytes,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, ByteRecord::with_capacity(FIELDS, CRLF_LEN), rows)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn csv_state(bytes: &'static [u8], rows: usize) -> CsvState {
    assert_eq!(bytes.len() % rows, 0, "corpus rows must have equal width");
    let row_width = bytes.len() / rows;
    let mut oracle = ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(&bytes[..row_width]));
    let mut oracle_record = ::csv::ByteRecord::with_capacity(CRLF_LEN, FIELDS);
    assert!(
        oracle
            .read_byte_record(&mut oracle_record)
            .unwrap_or_else(|error| panic!("benchmark oracle failed: {error}")),
        "benchmark oracle stopped before its row"
    );
    assert_eq!(
        oracle_record.len(),
        EXPECTED_FIELDS.len(),
        "wrong field count"
    );
    for (index, expected) in EXPECTED_FIELDS.iter().enumerate() {
        assert_eq!(
            oracle_record.get(index),
            Some(*expected),
            "wrong field {index}"
        );
    }
    assert!(
        !oracle
            .read_byte_record(&mut oracle_record)
            .unwrap_or_else(|error| panic!("benchmark oracle failed at EOF: {error}")),
        "benchmark oracle parsed extra records"
    );

    let reader = ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(bytes));
    (
        reader,
        ::csv::ByteRecord::with_capacity(CRLF_LEN, FIELDS),
        rows,
    )
}

fn sum(record: &ByteRecord) -> u64 {
    let mut total = 0_u64;
    for index in 0..record.len() {
        total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
    }
    total
}

fn run(state: State) -> (u64, SliceParser<'static, Dynamic>, ByteRecord) {
    let (mut parser, mut record, rows) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, rows)), parser, record)
}

macro_rules! dialect_case {
    ($name:ident, $build:ident, $c1:expr, $c10:expr, $c100:expr, $c1000:expr) => {
        #[library_benchmark]
        #[bench::rows_1(args = ($c1, 1), setup = $build, teardown = drop_it)]
        #[bench::rows_10(args = ($c10, 10), setup = $build, teardown = drop_it)]
        #[bench::rows_100(args = ($c100, 100), setup = $build, teardown = drop_it)]
        #[bench::rows_1000(args = ($c1000, 1000), setup = $build, teardown = drop_it)]
        fn $name(input: Input) -> (u64, SliceParser<'static, Dynamic>, ByteRecord) {
            run(state(input))
        }
    };
}

dialect_case!(specialized, baseline, ROWS_1, ROWS_10, ROWS_100, ROWS_1000);
dialect_case!(general_crlf, crlf, &CRLF_1, &CRLF_10, &CRLF_100, &CRLF_1000);
dialect_case!(
    general_mysql_escape,
    mysql_escape,
    ROWS_1,
    ROWS_10,
    ROWS_100,
    ROWS_1000
);

// ── Content that activates each option ──────────────────────────────────────

const TRIGGER_ROWS: usize = 1000;

const fn repeated<const N: usize>(row: &[u8]) -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        out[index] = row[index % row.len()];
        index += 1;
    }
    out
}

static MYSQL_ESCAPE: [u8; 11 * TRIGGER_ROWS] = repeated(b"one\\ttwo,x\n");
static NULL_POSTGRES: [u8; 8 * TRIGGER_ROWS] = repeated(b",value,\n");
static NULL_MYSQL: [u8; 9 * TRIGGER_ROWS] = repeated(b"\\N,value\n");
static TRIMMED: [u8; 14 * TRIGGER_ROWS] = repeated(b"  one  , two \n");
/// A quoted field alongside a padded unquoted one, so `Whitespace::unquoted_only`
/// cannot stay on the vectorized path: only the general parser knows which
/// field was quoted while it trims. Measured against `trim_quoted_control`,
/// which reads the same bytes under a dialect that asks for no trimming.
static TRIM_QUOTED: [u8; 14 * TRIGGER_ROWS] = repeated(b"\"one\",  two  \n");
static COMMENTED: [u8; 19 * TRIGGER_ROWS] = repeated(b"# ignored\none,two\n\n");
static MULTIBYTE: [u8; 13 * TRIGGER_ROWS] = repeated(b"one||two|||x\n");

type TriggerInput = (&'static [u8], FormatOptions, u64);
type TriggerState = (SliceParser<'static, Dynamic>, ByteRecord, u64);

fn trigger_state(input: TriggerInput) -> TriggerState {
    let (bytes, format, expected) = input;
    let parser = SliceParser::<Dynamic>::with_options(
        bytes,
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(BUFFER),
    )
    .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (
        parser,
        ByteRecord::with_capacity(FIELDS, CRLF_LEN),
        expected,
    )
}

fn run_triggered(state: TriggerState) -> (u64, SliceParser<'static, Dynamic>, ByteRecord) {
    let (mut parser, mut record, expected) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    (black_box(total), parser, record)
}

macro_rules! triggered_case {
    ($name:ident, $bytes:expr, $format:expr, $expected:expr) => {
        #[library_benchmark]
        #[bench::rows_1000(
                                                            args = (($bytes, $format, $expected)),
                                                            setup = trigger_state,
                                                            teardown = drop_it
                                                        )]
        fn $name(state: TriggerState) -> (u64, SliceParser<'static, Dynamic>, ByteRecord) {
            run_triggered(state)
        }
    };
}

triggered_case!(
    mysql_escape_triggered,
    &MYSQL_ESCAPE,
    FormatOptions::CSV
        .escape(Escape::Mysql)
        .quoting(Quoting::Never),
    (8 * TRIGGER_ROWS) as u64
);
triggered_case!(
    null_postgres_triggered,
    &NULL_POSTGRES,
    FormatOptions::CSV.nulls(Nulls::PostgresCsv),
    (5 * TRIGGER_ROWS) as u64
);
triggered_case!(
    null_mysql_triggered,
    &NULL_MYSQL,
    FormatOptions::CSV
        .escape(Escape::Mysql)
        .quoting(Quoting::Never)
        .nulls(Nulls::Mysql),
    (5 * TRIGGER_ROWS) as u64
);
triggered_case!(
    trim_triggered,
    &TRIMMED,
    FormatOptions::CSV.trim(Whitespace::FIELDS.unquoted_only()),
    (6 * TRIGGER_ROWS) as u64
);
triggered_case!(
    trim_quoted_fallback,
    &TRIM_QUOTED,
    FormatOptions::CSV.trim(Whitespace::FIELDS.unquoted_only()),
    (6 * TRIGGER_ROWS) as u64
);
triggered_case!(
    trim_quoted_control,
    &TRIM_QUOTED,
    FormatOptions::CSV,
    (10 * TRIGGER_ROWS) as u64
);
triggered_case!(
    crlf_triggered,
    &CRLF_1000,
    FormatOptions::CSV.record_ending(RecordEnding::CrLf),
    FIELD_BYTES * TRIGGER_ROWS as u64
);
triggered_case!(
    comments_triggered,
    &COMMENTED,
    FormatOptions::CSV
        .comment(Some(b'#'))
        .blank_records(coseva::config::BlankRecords::Skip),
    (6 * TRIGGER_ROWS) as u64
);
triggered_case!(
    multibyte_triggered,
    &MULTIBYTE,
    FormatOptions::CSV.delimiter_sequence(b"||"),
    (8 * TRIGGER_ROWS) as u64
);
dialect_case!(
    general_nulls_postgres,
    nulls_postgres,
    ROWS_1,
    ROWS_10,
    ROWS_100,
    ROWS_1000
);
dialect_case!(
    general_nulls_mysql,
    nulls_mysql,
    ROWS_1,
    ROWS_10,
    ROWS_100,
    ROWS_1000
);
dialect_case!(
    general_trim_unquoted,
    trim_unquoted,
    ROWS_1,
    ROWS_10,
    ROWS_100,
    ROWS_1000
);

// ── two reference points, measured here so they are directly comparable ──────

// The same bytes through the format-specialized parser the other suites use,
// which is what a caller gets when the format is known at compile time.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1, 1), setup = static_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10, 10), setup = static_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100, 100), setup = static_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000, 1000), setup = static_state, teardown = drop_it)]
fn specialized_static(state: StaticState) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
    let (mut parser, mut record, rows) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, rows)), parser, record)
}

// The `csv` crate over the same bytes, so the general-path rows can be read
// against something outside this crate rather than only against each other.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1, 1), setup = csv_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10, 10), setup = csv_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100, 100), setup = csv_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000, 1000), setup = csv_state, teardown = drop_it)]
fn csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>, ::csv::ByteRecord) {
    let (mut reader, mut record, rows) = state;
    let mut total = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, rows)), reader, record)
}

library_benchmark_group!(
    name = dialects;
    benchmarks =
        specialized,
        general_crlf,
        general_mysql_escape,
        general_nulls_postgres,
        general_nulls_mysql,
        general_trim_unquoted,
        specialized_static,
        csv
);

library_benchmark_group!(
    name = triggered;
    benchmarks =
        mysql_escape_triggered,
        null_postgres_triggered,
        null_mysql_triggered,
        trim_triggered,
        trim_quoted_fallback,
        trim_quoted_control,
        crlf_triggered,
        comments_triggered,
        multibyte_triggered
);

main!(library_benchmark_groups = dialects, triggered);
