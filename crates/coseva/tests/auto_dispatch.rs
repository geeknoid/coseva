//! The engine substitutes a static format for its own settings on its own.
//!
//! A parser configured at run time with settings that match a built-in format
//! runs that format's folded kernel instead of the general one. That is a
//! silent substitution, so it is only sound while the classifier is exact:
//! settings that differ from a built-in in *anything a folded accessor reads*
//! must fall back to the general kernel.
//!
//! The dangerous failure is a classifier that is too permissive. If, say, the
//! trim policy were left out of the comparison, a parser configured to trim
//! would be handed the plain `Csv` kernel, whose accessors fold trimming to
//! off, and it would silently stop trimming. Every case below is a setting one
//! step away from a built-in: each must survive the round trip unchanged.
//!
//! Each perturbation is checked against the *same* options declared through
//! `csv_format!`. A statically declared format folds its own constants by
//! construction, so it is the reference for what the run-time parser must do.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{
    BlankRecords, Escape, FormatOptions, Headers, Nulls, ParseOptions, Syntax, Whitespace,
};
use coseva::csv_format;
use coseva::{IoParser, PushParser, SliceParser};

/// A field is its bytes *and* whether it is NULL. A NULL field yields no
/// bytes, so recording only the bytes would make a lost NULL policy invisible.
type Field = (Vec<u8>, bool);
type Records = Result<Vec<Vec<Field>>, String>;

fn fields(record: &coseva::Record<'_>) -> Vec<Field> {
    (0..record.len())
        .map(|index| {
            (
                record.get(index).unwrap_or_default().to_vec(),
                record.is_null(index) == Some(true),
            )
        })
        .collect()
}

fn collect<F: coseva::format::CsvFormat>(parser: &mut SliceParser<'_, F>) -> Records {
    let mut out = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                // `record()` is the borrowed path, which is the one the
                // engine's format substitution actually serves. Collecting
                // through `read_byte_record_into` would exercise the owned
                // parser instead and never reach the substitution at all.
                let record = line.record().map_err(|error| error.to_string())?;
                out.push(fields(&record));
            }
            Ok(None) => return Ok(out),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn parse_options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

/// Parse with run-time options, which is where the engine may substitute.
fn dynamic(input: &[u8], options: FormatOptions) -> Records {
    let mut parser = SliceParser::with_options(input, options, parse_options())
        .map_err(|error| error.to_string())?;
    collect(&mut parser)
}

/// Parse with a compile-time format, which folds its own constants.
fn statik<F: coseva::format::StaticFormat>(input: &[u8]) -> Records {
    let mut parser =
        SliceParser::<F>::new(input, parse_options()).map_err(|error| error.to_string())?;
    collect(&mut parser)
}

fn stream_dynamic(input: &[u8], options: FormatOptions) -> Records {
    // A tiny buffer puts a window seam inside almost every record, which is
    // where a substituted kernel and the general one would drift apart first.
    let mut parser = IoParser::with_options(input, options, parse_options().buffer_capacity(32))
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                // `record()` is the borrowed path, which is the one the
                // engine's format substitution actually serves. Collecting
                // through `read_byte_record_into` would exercise the owned
                // parser instead and never reach the substitution at all.
                let record = line.record().map_err(|error| error.to_string())?;
                out.push(fields(&record));
            }
            Ok(None) => return Ok(out),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn push_dynamic(input: &[u8], options: FormatOptions) -> Records {
    let mut parser =
        PushParser::with_options(options, parse_options()).map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    // A byte at a time, so every record crosses a chunk boundary.
    for bytes in input.chunks(1) {
        let mut fed = 0;
        while fed < bytes.len() {
            fed += drain_push(&mut parser, &bytes[fed..], &mut out)?;
        }
    }
    parser.finish();
    let _ = drain_push(&mut parser, b"", &mut out)?;
    Ok(out)
}

/// Lend one chunk, collect the records it completes, and report the take.
fn drain_push<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    out: &mut Vec<Vec<Field>>,
) -> Result<usize, String> {
    let mut chunk = parser.chunk(input);
    loop {
        match chunk.next_line() {
            Ok(Some(mut line)) => {
                // `record()` is the borrowed path, which is the one the
                // engine's format substitution actually serves. Collecting
                // through `read_byte_record_into` would exercise the owned
                // parser instead and never reach the substitution at all.
                let record = line.record().map_err(|error| error.to_string())?;
                out.push(fields(&record));
            }
            Ok(None) => break,
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(chunk.done())
}

/// Inputs that exercise the settings the classifier compares: quoting,
/// escaping, surrounding space, blank records and NULL sentinels.
fn corpus() -> Vec<Vec<u8>> {
    let mut inputs: Vec<Vec<u8>> = vec![
        b"a,b,c\n".to_vec(),
        b" a , b , c \n".to_vec(),
        b"\"a\",\" b \",c\n".to_vec(),
        b"\"a\"\"b\",c\n".to_vec(),
        b"a,,c\n\n b,c,d\n".to_vec(),
        b"\\N,a,\\N\n".to_vec(),
        b"a,\"multi\nline\",c\n".to_vec(),
        b"a,b\\,c,d\n".to_vec(),
        b"'a','b'\n".to_vec(),
        b"a;b;c\n".to_vec(),
        b"a\tb\tc\n".to_vec(),
        b"a|b|c\n".to_vec(),
        b"a^b^c\n".to_vec(),
        b"\n\n\n".to_vec(),
        b"".to_vec(),
        // Malformed quoting, which `Syntax` alone decides between rejecting
        // and recovering from. Without these the syntax setting is invisible.
        b"a,b\"c,d\n".to_vec(),
        b"\"a\"b,c\n".to_vec(),
        b"\"a\" ,b\n".to_vec(),
        b"\"unterminated,b\n".to_vec(),
        b"\"a\"\"\"b\",c\n".to_vec(),
        b"a,\"b\"x\"c\",d\n".to_vec(),
        // Space after a delimiter, which `skip_initial_space` decides on and
        // which trimming would otherwise mask.
        b"a, b,  c\n".to_vec(),
        b"a, \"b\" ,c\n".to_vec(),
        // Blank and whitespace-only records, which `blank_records` decides on.
        b"a,b\n\nc,d\n".to_vec(),
        b"a,b\n \nc,d\n".to_vec(),
        b"\n a,b\n".to_vec(),
        // NULL sentinels in every position, quoted and bare.
        b"\\N\n".to_vec(),
        b"a,\\N,b\n".to_vec(),
        b"\"\\N\",a\n".to_vec(),
        b"NULL,\\N,\n".to_vec(),
    ];

    // A deterministic generator mixes field widths so record boundaries land
    // on every alignment the kernels care about.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..400 {
        let mut input = Vec::new();
        for _ in 0..=(next() % 4) {
            for field in 0..=(next() % 5) {
                if field > 0 {
                    input.push(b',');
                }
                match next() % 7 {
                    0 => input.extend_from_slice(&b"y".repeat((next() % 33) as usize)),
                    1 => input.extend_from_slice(b"\"quoted, value\""),
                    2 => input.extend_from_slice(b"\"esc\"\"aped\""),
                    3 => input.extend_from_slice(b"  padded  "),
                    4 => input.extend_from_slice(b"\\N"),
                    5 => input.extend_from_slice(b"bad\"quote"),
                    _ => {}
                }
            }
            input.push(b'\n');
        }
        inputs.push(input);
    }
    inputs
}

/// Assert a run-time configuration behaves exactly like the same format
/// declared at compile time, across all three parser front ends.
fn assert_agrees<F: coseva::format::StaticFormat>(options: FormatOptions, label: &str) {
    for input in corpus() {
        let shown = String::from_utf8_lossy(&input).into_owned();
        assert_eq!(
            dynamic(&input, options),
            statik::<F>(&input),
            "{label}: run-time and compile-time slice parsing diverged on {shown:?}"
        );
        assert_eq!(
            stream_dynamic(&input, options),
            statik::<F>(&input),
            "{label}: streaming diverged from compile-time slice parsing on {shown:?}"
        );
        assert_eq!(
            push_dynamic(&input, options),
            statik::<F>(&input),
            "{label}: push diverged from compile-time slice parsing on {shown:?}"
        );
    }
}

csv_format! {
    /// The exact built-in shape, which the engine is expected to substitute.
    pub PlainCsv = FormatOptions::CSV;
    /// Tab separated, the other shape the engine substitutes.
    pub PlainTsv = FormatOptions::TSV;

    // Each of the following is CSV in everything but one setting a folded
    // accessor reads. None may be mistaken for CSV.
    /// CSV that trims surrounding whitespace.
    pub CsvTrimmed = FormatOptions::CSV.trim(Whitespace::ALL);
    /// CSV that skips the space after a delimiter.
    pub CsvSkipSpace = FormatOptions::CSV.skip_initial_space(true);
    /// CSV that reads MySQL NULL sentinels.
    pub CsvNulls = FormatOptions::CSV.nulls(Nulls::Mysql);
    /// CSV that drops blank records instead of preserving them.
    pub CsvSkipBlanks = FormatOptions::CSV.blank_records(BlankRecords::Skip);
    /// CSV that recovers from malformed quoting instead of rejecting it.
    pub CsvCompatible = FormatOptions::CSV.syntax(Syntax::Compatible(coseva::config::Recovery::PERMISSIVE));
    /// CSV with a different delimiter.
    pub CsvCaret = FormatOptions::CSV.delimiter(b'^');
    /// CSV with a different quote.
    pub CsvApostrophe = FormatOptions::CSV.quote(b'\'');
    /// CSV that escapes with a backslash rather than doubling.
    pub CsvBackslash = FormatOptions::CSV.escape(Escape::Backslash(b'\\'));

    // And one step away from TSV, which is the second substitution arm.
    /// TSV that trims surrounding whitespace.
    pub TsvTrimmed = FormatOptions::TSV.trim(Whitespace::ALL);
    /// TSV that reads MySQL NULL sentinels.
    pub TsvNulls = FormatOptions::TSV.nulls(Nulls::Mysql);
}

/// The two shapes the engine is expected to substitute must still be correct.
///
/// These are the cases where the substitution actually fires, so they check
/// that folding CSV and TSV constants changes nothing observable.
#[test]
fn substituted_formats_behave_like_their_static_twins() {
    assert_agrees::<PlainCsv>(FormatOptions::CSV, "csv");
    assert_agrees::<PlainTsv>(FormatOptions::TSV, "tsv");
}

/// Settings one step from CSV must not be treated as CSV.
///
/// Each case differs from `FormatOptions::CSV` in exactly one setting that a
/// folded accessor reads. If the classifier ignored that setting, the parser
/// would run the plain CSV kernel and quietly lose the behaviour.
#[test]
fn near_miss_settings_keep_their_own_behaviour() {
    assert_agrees::<CsvTrimmed>(FormatOptions::CSV.trim(Whitespace::ALL), "csv+trim");
    assert_agrees::<CsvSkipSpace>(
        FormatOptions::CSV.skip_initial_space(true),
        "csv+skip_initial_space",
    );
    assert_agrees::<CsvNulls>(FormatOptions::CSV.nulls(Nulls::Mysql), "csv+nulls");
    assert_agrees::<CsvSkipBlanks>(
        FormatOptions::CSV.blank_records(BlankRecords::Skip),
        "csv+blank_records",
    );
    assert_agrees::<CsvCompatible>(
        FormatOptions::CSV.syntax(Syntax::Compatible(coseva::config::Recovery::PERMISSIVE)),
        "csv+compatible",
    );
    assert_agrees::<CsvCaret>(FormatOptions::CSV.delimiter(b'^'), "csv+delimiter");
    assert_agrees::<CsvApostrophe>(FormatOptions::CSV.quote(b'\''), "csv+quote");
    assert_agrees::<CsvBackslash>(
        FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
        "csv+escape",
    );
}

/// The same, one step from TSV, so the second substitution arm is covered.
#[test]
fn near_miss_tsv_settings_keep_their_own_behaviour() {
    assert_agrees::<TsvTrimmed>(FormatOptions::TSV.trim(Whitespace::ALL), "tsv+trim");
    assert_agrees::<TsvNulls>(FormatOptions::TSV.nulls(Nulls::Mysql), "tsv+nulls");
}

/// Resetting a parser must not disturb its classification.
///
/// `reset` rebuilds the engine's cached derived state, including the format
/// classification, from the retained settings. A reset that recomputed it
/// wrongly, or forgot to recompute it at all, would leave a reused parser
/// running the wrong kernel on its second stream but not its first.
#[test]
fn a_reset_parser_keeps_parsing_its_own_format() {
    let cases: [(FormatOptions, &[u8], Records); 3] = [
        (
            FormatOptions::CSV,
            b"a,\"b,b\",c\n",
            statik::<PlainCsv>(b"a,\"b,b\",c\n"),
        ),
        (
            FormatOptions::CSV.delimiter(b'^'),
            b"a^\"b^b\"^c\n",
            statik::<CsvCaret>(b"a^\"b^b\"^c\n"),
        ),
        (
            FormatOptions::CSV.trim(Whitespace::ALL),
            b" a , b , c \n",
            statik::<CsvTrimmed>(b" a , b , c \n"),
        ),
    ];

    for (options, input, expected) in cases {
        let mut parser = PushParser::with_options(options, parse_options()).expect("parser");
        let mut first = Vec::new();
        let mut fed = 0;
        while fed < input.len() {
            fed += drain_push(&mut parser, &input[fed..], &mut first).expect("drain");
        }
        parser.finish();
        let _ = drain_push(&mut parser, b"", &mut first).expect("drain");
        assert_eq!(Ok(first), expected, "first pass diverged");

        parser.reset();

        let mut second = Vec::new();
        let mut fed = 0;
        while fed < input.len() {
            fed += drain_push(&mut parser, &input[fed..], &mut second).expect("drain");
        }
        parser.finish();
        let _ = drain_push(&mut parser, b"", &mut second).expect("drain");
        assert_eq!(Ok(second), expected, "a reset parser changed behaviour");
    }
}

/// Every built-in [`StaticFormat`] must match the constant it is named for.
///
/// The two are declared in different places -- the type in `format`, the
/// value in `config` -- so nothing but this test stops them drifting apart.
/// A mismatch would specialize a parser for a format the caller did not ask
/// for, which is silent rather than an error.
#[test]
fn built_in_static_formats_match_their_constants() {
    use coseva::format::{
        BackslashCsv, BackslashTsv, CommentedCsv, Excel, Mysql, Pipe, PostgresCopyCsv, PythonCsv,
        Rfc4180, Semicolon, TrimmedCsv,
    };

    assert_agrees::<Semicolon>(FormatOptions::SEMICOLON, "semicolon");
    assert_agrees::<Pipe>(FormatOptions::PIPE, "pipe");
    assert_agrees::<BackslashCsv>(FormatOptions::BACKSLASH_CSV, "backslash_csv");
    assert_agrees::<BackslashTsv>(FormatOptions::BACKSLASH_TSV, "backslash_tsv");
    assert_agrees::<CommentedCsv>(FormatOptions::COMMENTED_CSV, "commented_csv");
    assert_agrees::<TrimmedCsv>(FormatOptions::TRIMMED_CSV, "trimmed_csv");
    assert_agrees::<PythonCsv>(FormatOptions::PYTHON_CSV, "python_csv");
    assert_agrees::<Rfc4180>(FormatOptions::RFC4180, "rfc4180");
    assert_agrees::<Excel>(FormatOptions::EXCEL, "excel");
    assert_agrees::<PostgresCopyCsv>(FormatOptions::POSTGRES_COPY_CSV, "postgres_copy_csv");
    assert_agrees::<Mysql>(FormatOptions::MYSQL, "mysql");
}
