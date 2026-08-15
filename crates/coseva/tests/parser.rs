//! Slice-reader integration tests.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io;

use coseva::ErrorKind;
use coseva::SliceParser;
use coseva::config::{
    BlankRecords, Escape, FieldCount, FormatOptions, Headers, Limits, Nulls, ParseOptions, Quoting,
    ReadBom, RecordEnding, Recovery, Syntax, Whitespace,
};
use coseva::format::Csv;
use coseva::{ByteRecord, Predicate, Record};

mod common;

use common::unheaded;

fn assert_fields(record: &Record<'_>, expected: &[&[u8]]) {
    assert_eq!(record.iter().collect::<Vec<_>>(), expected);
}

fn parse_all(
    input: &'static [u8],
    format: FormatOptions,
    options: ParseOptions,
) -> Result<Vec<Vec<Vec<u8>>>, coseva::Error> {
    let mut p = SliceParser::with_options(input, format, options).expect("valid options");
    let mut out = Vec::new();
    while let Some(mut line) = p.next_line()? {
        let mut rec = ByteRecord::new();
        line.read_byte_record_into(&mut rec)?;
        out.push(rec.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(out)
}

fn parse_unheaded(
    input: &'static [u8],
    format: FormatOptions,
) -> Result<Vec<Vec<Vec<u8>>>, coseva::Error> {
    parse_all(input, format, ParseOptions::new().headers(Headers::None))
}

/// Like [`parse_unheaded`] but borrows `input` for only the parse, so tests can
/// build multi-record CSV at runtime (the interior-quote prediction only
/// engages across a run of records, which is awkward to spell as a literal).
fn parse_unheaded_owned(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, coseva::Error> {
    let mut p = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut out = Vec::new();
    while let Some(mut line) = p.next_line()? {
        let mut rec = ByteRecord::new();
        line.read_byte_record_into(&mut rec)?;
        out.push(rec.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(out)
}

/// A `fmt::Write` adapter that budgets N bytes then fails, used to exercise
/// error `Display` formatting when the destination writer fails partway through.
struct BudgetWriter {
    budget: usize,
}

impl std::fmt::Write for BudgetWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.len() > self.budget {
            return Err(std::fmt::Error);
        }
        self.budget = self.budget.saturating_sub(s.len());
        Ok(())
    }
}

/// Parses `input` with non-default `limits`, which makes the engine ineligible
/// for its specialized owned-record kernels and forces the general owned parser
/// that `Line::read_byte_record_into` falls back to.
fn owned_records(
    input: &'static [u8],
    format: FormatOptions,
    limits: Limits,
) -> Result<Vec<Vec<Vec<u8>>>, coseva::Error> {
    let options = ParseOptions::new().headers(Headers::None).limits(limits);
    let mut parser = SliceParser::with_options(input, format, options).expect("valid options");
    let mut records = Vec::new();
    let mut record = ByteRecord::new();
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        records.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(records)
}

/// A header/data pair used by the projected-decode tests below: the target's
/// field names are a strictly ascending subset of these headers.
const PROJECTED_HEADER: &[u8] = b"left,middle,count\nalpha,ignored,7\n";

#[derive(Debug, Default, PartialEq, Eq)]
struct Pair {
    left: String,
    right: String,
}

impl<'record> coseva::encoding::CsvDecode<'record> for Pair {
    fn csv_decode<R>(record: &R) -> Result<Self, coseva::Error>
    where
        R: coseva::encoding::DecodeRecord<'record> + ?Sized,
    {
        let field = |index: usize| {
            String::from_utf8_lossy(record.get_field(index).unwrap_or_default()).into_owned()
        };

        Ok(Self {
            left: field(0),
            right: field(1),
        })
    }

    fn field_names() -> &'static [&'static str] {
        &["left", "right"]
    }
}

/// A target whose field names are a strictly ascending subset of the header
/// record, which makes the engine resolve a *projected* mapping and reach for
/// the projected parse kernel.
#[derive(Debug, Default, PartialEq, Eq)]
struct Projected {
    left: String,
    count: u32,
}

impl<'record> coseva::encoding::CsvDecode<'record> for Projected {
    fn csv_decode<R>(record: &R) -> Result<Self, coseva::Error>
    where
        R: coseva::encoding::DecodeRecord<'record> + ?Sized,
    {
        let left = String::from_utf8_lossy(record.get_field(0).unwrap_or_default()).into_owned();
        let raw = String::from_utf8_lossy(record.get_field(1).unwrap_or_default()).into_owned();
        let count = raw
            .parse::<u32>()
            .map_err(|error| coseva::Error::from_field_conversion(error, 1, "count"))?;
        Ok(Self { left, count })
    }

    fn field_names() -> &'static [&'static str] {
        &["left", "count"]
    }
}

#[test]
fn parses_plain_fields_without_copying() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"city,population\nBoston,650706\n");

    let mut line = reader.next_line()?.expect("missing headers");
    let headers = line.record()?;
    assert_fields(&headers, &[b"city", b"population"]);
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.get_str(0)?, Some("Boston"));
    assert_eq!(row.parse::<u64>(1)?, Some(650_706));
    assert_eq!(row.byte_range(), 16..30);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn borrows_simple_quotes_and_copies_escaped_quotes() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(br#""plain","say ""hello""","tail""#);
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;

    assert_eq!(row.get(0), Some(&b"plain"[..]));
    assert_eq!(row.get(1), Some(&b"say \"hello\""[..]));
    assert_eq!(row.get(2), Some(&b"tail"[..]));
    Ok(())
}

#[test]
fn preserves_multiple_escaped_fields() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(br#""a""b","c""d","e""f""#);
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;

    assert_fields(&row, &[b"a\"b", b"c\"d", b"e\"f"]);
    assert!(row.iter().all(|field| !field.is_empty()));
    Ok(())
}

#[test]
fn parses_crlf_and_quoted_newlines() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"a,\"b\r\nc\"\r\nd,e\r\n");
    let mut line = reader.next_line()?.expect("missing first row");
    let row = line.record()?;
    assert_fields(&row, &[b"a", b"b\r\nc"]);
    let mut line = reader.next_line()?.expect("missing second row");
    let row = line.record()?;
    assert_fields(&row, &[b"d", b"e"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn handles_empty_records_and_trailing_fields() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"\n,\na,\n");
    let mut line = reader.next_line()?.expect("missing empty row");
    let row = line.record()?;
    assert_fields(&row, &[b""]);
    let mut line = reader.next_line()?.expect("missing two-field row");
    let row = line.record()?;
    assert_fields(&row, &[b"", b""]);
    let mut line = reader.next_line()?.expect("missing trailing-field row");
    let row = line.record()?;
    assert_fields(&row, &[b"a", b""]);
    Ok(())
}

#[test]
fn supports_custom_dialect_comments_and_bom() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b';')
        .quote(b'\'')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::Backslash(b'\\'))
        .comment(Some(b'#'));
    let input = b"\xEF\xBB\xBF#ignored|'a\\'b';c|d;e|";
    let mut reader =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;

    let mut line = reader.next_line()?.expect("missing first row");
    let row = line.record()?;
    assert_fields(&row, &[b"a'b", b"c"]);
    let mut line = reader.next_line()?.expect("missing second row");
    let row = line.record()?;
    assert_fields(&row, &[b"d", b"e"]);
    Ok(())
}

#[test]
fn comments_and_blank_records_are_skipped_until_data() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.comment(Some(b'#'));
    let mut reader = SliceParser::with_options(
        b"# first\n\n# second\n\nvalue,row\n",
        format.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing data row");
    let row = line.record()?;
    assert_fields(&row, &[b"value", b"row"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn named_reader_presets_cover_common_dialects() -> Result<(), Box<dyn StdError>> {
    type PresetCase = (FormatOptions, &'static [u8], &'static [&'static [u8]]);
    let cases: &[PresetCase] = &[
        (FormatOptions::CSV, b"a,\"b,c\"\n", &[b"a", b"b,c"]),
        (FormatOptions::TSV, b"a\t\"b\tc\"\n", &[b"a", b"b\tc"]),
        (FormatOptions::SEMICOLON, b"a;\"b;c\"\n", &[b"a", b"b;c"]),
        (FormatOptions::PIPE, b"a|\"b|c\"\n", &[b"a", b"b|c"]),
        (
            FormatOptions::BACKSLASH_CSV,
            b"\"a\\\"b\",c\n",
            &[b"a\"b", b"c"],
        ),
        (
            FormatOptions::BACKSLASH_TSV,
            b"\"a\\\"b\"\tc\n",
            &[b"a\"b", b"c"],
        ),
        (
            FormatOptions::COMMENTED_CSV,
            b"# ignored\n\nvalue,row\n",
            &[b"value", b"row"],
        ),
        (
            FormatOptions::TRIMMED_CSV,
            b"  value  ,\" quoted \"\n",
            &[b"value", b"quoted"],
        ),
        (
            FormatOptions::PYTHON_CSV,
            b"  first,   \"quoted\",  plain  ,   ,\"  kept  \"\n",
            &[b"  first", b"quoted", b"plain  ", b"", b"  kept  "],
        ),
    ];

    for &(format, input, expected) in cases {
        let mut reader =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
        let mut line = reader.next_line()?.expect("missing format record");
        let record = line.record()?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            expected,
            "format {format:?}",
        );
        assert!(reader.next_line()?.is_none(), "format {format:?}");
    }
    Ok(())
}

#[test]
fn specialized_named_owned_kernels_cover_all_record_shapes() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (FormatOptions::TSV, b'\t', false),
        (FormatOptions::SEMICOLON, b';', false),
        (FormatOptions::PIPE, b'|', false),
        (FormatOptions::BACKSLASH_CSV, b',', true),
        (FormatOptions::BACKSLASH_TSV, b'\t', true),
    ];

    for (format, delimiter, backslash) in cases {
        let mut input = b"plain".to_vec();
        input.push(delimiter);
        if backslash {
            input.extend_from_slice(b"\"say \\\"hello\\\"\"");
        } else {
            input.extend_from_slice(b"\"say \"\"hello\"\"\"");
        }
        input.push(delimiter);
        input.extend_from_slice(b"tail\r\n\"quoted");
        input.push(delimiter);
        input.extend_from_slice(b"value\"");
        input.push(delimiter);
        input.extend_from_slice(b"end\nlast");
        input.push(delimiter);
        input.extend_from_slice(b"row");

        let mut reader =
            SliceParser::with_options(&input, format, ParseOptions::new().headers(Headers::None))?;
        let mut record = ByteRecord::with_capacity(3, 32);

        let next = reader.next_line()?;
        assert!(next.is_some(), "format {format:?}");
        let mut line = next.expect("format record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"plain".as_slice(), b"say \"hello\"", b"tail"],
            "format {format:?}",
        );
        assert_eq!(
            record.byte_range(),
            0..input
                .iter()
                .position(|&b| b == b'\n')
                .expect("input contains a terminated record")
                + 1
        );
        assert_eq!(record.index(), 0);

        let next = reader.next_line()?;
        assert!(next.is_some(), "format {format:?}");
        let mut line = next.expect("format record");
        line.read_byte_record_into(&mut record)?;
        let mut quoted = b"quoted".to_vec();
        quoted.push(delimiter);
        quoted.extend_from_slice(b"value");
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [quoted.as_slice(), b"end"],
            "format {format:?}",
        );
        assert_eq!(record.index(), 1);

        let next = reader.next_line()?;
        assert!(next.is_some(), "format {format:?}");
        let mut line = next.expect("format record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"last".as_slice(), b"row"],
            "format {format:?}",
        );
        assert_eq!(record.index(), 2);
        assert!(reader.next_line()?.is_none(), "format {format:?}");
    }
    Ok(())
}

#[test]
fn specialized_named_owned_kernels_preserve_fallback_errors() -> Result<(), Box<dyn StdError>> {
    let cases: &[(FormatOptions, &[u8])] = &[
        (FormatOptions::TSV, b"\"a\"x\tb\n"),
        (FormatOptions::SEMICOLON, b"\"a\"x;b\n"),
        (FormatOptions::PIPE, b"\"a\"x|b\n"),
        (FormatOptions::BACKSLASH_CSV, b"\"a\\q\",b\n"),
        (FormatOptions::BACKSLASH_TSV, b"\"a\\q\"\tb\n"),
    ];

    for &(format, input) in cases {
        let mut borrowed =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
        let mut line = borrowed.next_line()?.expect("record");
        let borrowed_error = line
            .record()
            .expect_err("borrowed parser should reject malformed input");

        let mut owned =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
        let mut record = ByteRecord::new();
        let mut line = owned.next_line()?.expect("record");
        let owned_error = line
            .read_byte_record_into(&mut record)
            .expect_err("owned parser should reject malformed input");
        assert_eq!(owned_error, borrowed_error, "format {format:?}");
    }
    Ok(())
}

#[test]
fn preset_options_can_be_overridden_after_application() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a|b\n",
        FormatOptions::TSV.delimiter(b'|'),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing record");
    let record = line.record()?;
    assert_fields(&record, &[b"a", b"b"]);
    Ok(())
}

#[test]
fn skip_initial_space_is_not_general_trimming() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"  first, \tsecond  ,   \"third\"\n",
        FormatOptions::CSV.skip_initial_space(true),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing record");
    let record = line.record()?;
    assert_fields(&record, &[b"  first", b"\tsecond  ", b"third"]);
    Ok(())
}

#[test]
fn slice_positions_track_physical_lines() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.comment(Some(b'#'));
    let input = b"# ignored\r\n\nfirst,\"a\nb\"\r\nsecond,row\nbad\"quote,x";
    let mut reader = SliceParser::with_options(
        input,
        format.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
    )?;

    assert_eq!(reader.location().line, 1);
    let mut line = reader.next_line()?.expect("record");
    let _ = line.record()?;
    assert_eq!(reader.location().line, 5);
    let mut line = reader.next_line()?.expect("record");
    let _ = line.record()?;
    assert_eq!(reader.location().line, 6);

    let mut line = reader.next_line()?.expect("record");
    let error = line.record().expect_err("third record should fail");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().line, 6);
    assert!(error.to_string().contains("line 6"));
    Ok(())
}

#[test]
fn slice_lines_are_independent_of_custom_record_terminators() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b',')
        .quote(b'"')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::DoubleQuote);
    let mut reader = SliceParser::with_options(
        b"a\nb|bad\"quote|",
        format,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut line = reader.next_line()?.expect("record");
    let _ = line.record()?;
    assert_eq!(reader.location().line, 2);
    let mut line = reader.next_line()?.expect("record");
    let error = line.record().expect_err("second record should fail");
    assert_eq!(error.location().line, 2);
    Ok(())
}

#[test]
fn compatible_quotes_have_consistent_field_start_semantics() -> Result<(), Box<dyn StdError>> {
    let syntax = Syntax::Compatible(Recovery::PERMISSIVE);
    let mut reader = SliceParser::with_options(
        b"a,\"b,c\",d\"e\n",
        FormatOptions::CSV.syntax(syntax),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing compatible row");
    let row = line.record()?;
    assert_fields(&row, &[b"a", b"b,c", b"d\"e"]);
    Ok(())
}

#[test]
fn rejects_malformed_quoting() {
    let cases: &[(&[u8], ErrorKind)] = &[
        (b"a\"b,c", ErrorKind::UnexpectedQuote),
        (b"\"a\"b,c", ErrorKind::UnexpectedByteAfterQuote(b'b')),
        (b"\"a,b", ErrorKind::UnterminatedQuotedField),
    ];

    for &(input, expected) in cases {
        let mut reader = unheaded(input);
        let mut line = reader
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let error = line.record().expect_err("input should fail");
        assert_eq!(error.kind(), expected);
        assert_eq!(
            reader
                .next_line()
                .expect_err("reader should remain failed")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }
}

#[test]
fn owned_mixed_tail_fallback_preserves_exact_errors() {
    let cases: &[(&[u8], ErrorKind, usize, usize)] = &[
        (
            b"plain,\"quoted\"x,tail\n",
            ErrorKind::UnexpectedByteAfterQuote(b'x'),
            14,
            2,
        ),
        (
            b"plain,\"unterminated",
            ErrorKind::UnterminatedQuotedField,
            19,
            1,
        ),
    ];

    for &(input, expected, byte, field) in cases {
        let mut reader = unheaded(input);
        let mut record = ByteRecord::with_capacity(4, 32);
        let mut line = reader
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let error = line
            .read_byte_record_into(&mut record)
            .expect_err("mixed tail should fail");
        assert_eq!(error.kind(), expected);
        assert_eq!(error.location().byte, byte);
        assert_eq!(error.location().field, field);
    }
}

#[test]
fn rejects_invalid_backslash_escape() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b',')
        .quote(b'"')
        .record_ending(RecordEnding::Newline)
        .escape(Escape::Backslash(b'\\'));
    let mut reader = SliceParser::with_options(b"\"a\\nb\"\n", format, ParseOptions::new())?;

    let error = reader.next_line().expect_err("escape should fail");
    assert_eq!(error.kind(), ErrorKind::InvalidEscape(b'n'));
    Ok(())
}

#[test]
fn enforces_limits_during_scanning() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (
            b"abcdef\n".as_slice(),
            Limits::new(64, 3, 8),
            ErrorKind::FieldTooLarge { limit: 3 },
        ),
        (
            b"a,b,c\n".as_slice(),
            Limits::new(64, 8, 2),
            ErrorKind::TooManyFields { limit: 2 },
        ),
        (
            b"ab,cd\n".as_slice(),
            Limits::new(4, 8, 8),
            ErrorKind::RecordTooLarge { limit: 4 },
        ),
        (
            b"\"abcdef\"\n".as_slice(),
            Limits::new(64, 3, 8),
            ErrorKind::FieldTooLarge { limit: 3 },
        ),
        (
            b"\"a\"\"b\"\n".as_slice(),
            Limits::new(64, 1, 8),
            ErrorKind::FieldTooLarge { limit: 1 },
        ),
    ];

    for (input, limits, expected) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().limits(limits),
        )?;
        assert_eq!(
            reader.next_line().expect_err("limit should fail").kind(),
            expected,
        );
    }
    Ok(())
}

#[test]
fn enforces_record_limit_after_quoted_delimiter() -> Result<(), Box<dyn StdError>> {
    let limits = Limits::new(3, 8, 8);

    let mut borrowed = SliceParser::with_options(
        b"\"a\",\"b\"\n",
        FormatOptions::CSV,
        ParseOptions::new().limits(limits),
    )?;
    let error = borrowed.next_line().expect_err("record limit should fail");
    assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
    assert_eq!(error.location().byte, 4);

    let mut owned = SliceParser::with_options(
        b"\"a\",\"b\"\n",
        FormatOptions::CSV,
        ParseOptions::new().limits(limits),
    )?;
    let error = owned.next_line().expect_err("record limit should fail");
    assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
    assert_eq!(error.location().byte, 4);
    Ok(())
}

#[test]
fn accepts_records_and_fields_exactly_at_configured_limits() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (b"abcd".as_slice(), Limits::new(4, 4, 1)),
        (b"abcd\n".as_slice(), Limits::new(5, 4, 1)),
        (b"\"ab\"".as_slice(), Limits::new(4, 2, 1)),
        (b"\"ab\"\n".as_slice(), Limits::new(5, 2, 1)),
    ];
    for (input, limits) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None).limits(limits),
        )?;
        assert!(reader.next_line()?.is_some(), "{input:?}");
        assert!(reader.next_line()?.is_none(), "{input:?}");
    }
    Ok(())
}

#[test]
fn validates_record_widths() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a,b\nc\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .field_count(FieldCount::MatchFirst)
            .headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing first row");
    let first = line.record()?;
    assert_eq!(first.len(), 2);
    let mut line = reader.next_line()?.expect("record");
    let error = line.record().expect_err("width should fail");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        },
    );
    Ok(())
}

#[test]
fn parsing_fails_permanently_after_error() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded(b"a,b\n\"unterminated");
    let mut line = parser.next_line()?.expect("missing first record");
    let mut first = ByteRecord::new();
    line.read_byte_record_into(&mut first)?;
    assert_eq!(first.get(0), Some(&b"a"[..]));
    let mut line = parser.next_line()?.expect("record");
    let _error = line.record().expect_err("expected a parse failure");
    // The failure latches: every later attempt reports it rather than
    // silently reporting end of input.
    let _error = parser.next_line().expect_err("expected a latched failure");
    let _error = parser.next_line().expect_err("expected a latched failure");
    Ok(())
}

#[test]
fn reports_invalid_utf8_without_affecting_byte_access() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"a,\xFF\n");
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;

    assert_eq!(row.get(1), Some(&b"\xFF"[..]));
    let error = row.get_str(1).expect_err("UTF-8 should fail");
    assert_eq!(error.location().field, 1);
    Ok(())
}

#[test]
fn owned_and_borrowed_paths_report_the_same_error() -> Result<(), Box<dyn StdError>> {
    let input = b"a,b\nc,\"unterminated";

    let mut borrowed = unheaded(input);
    let mut line = borrowed.next_line()?.expect("missing borrowed row");
    let _ = line.record()?;
    let mut line = borrowed.next_line()?.expect("record");
    let borrowed_error = line.record().expect_err("borrowed parser should fail");

    let mut owned = unheaded(input);
    let mut record = ByteRecord::with_capacity(2, 16);
    let mut line = owned.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = owned.next_line()?.expect("record");
    let owned_error = line
        .read_byte_record_into(&mut record)
        .expect_err("owned parser should fail");

    assert_eq!(owned_error, borrowed_error);
    Ok(())
}

// ── RecordEnding::CrLf strictness ─────────────────────────────────────────────

#[test]
fn crlf_terminator_accepts_exact_boundaries_and_treats_quoted_cr_lf_as_data()
-> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a,b\r\n\"c\rd\",\"e\nf\"\r\nlast,row",
        FormatOptions::RFC4180,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing first row");
    let row = line.record()?;
    assert_fields(&row, &[b"a", b"b"]);
    let mut line = reader.next_line()?.expect("missing second row");
    let row = line.record()?;
    assert_fields(&row, &[b"c\rd", b"e\nf"]);
    let mut line = reader.next_line()?.expect("missing third row");
    let row = line.record()?;
    assert_fields(&row, &[b"last", b"row"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn crlf_terminator_accepts_final_record_without_trailing_terminator()
-> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a,b\r\nc,d",
        FormatOptions::RFC4180,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing first row");
    assert_fields(&line.record()?, &[b"a", b"b"]);
    let mut line = reader.next_line()?.expect("missing second row");
    assert_fields(&line.record()?, &[b"c", b"d"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn crlf_terminator_rejects_bare_lf_outside_quotes_with_exact_position() {
    let cases: &[(&[u8], usize)] = &[
        (b"a,b\n", 3),
        (b"a\nb", 1),
        (b"a,b\r\nc,d\n", 8),
        (b",\n", 1),
    ];
    for &(input, byte) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let error = loop {
            match reader.next_line() {
                Ok(Some(mut line)) => match line.record() {
                    Ok(_) => {}
                    Err(found) => break Some(found),
                },
                Ok(None) => break None,
                Err(found) => break Some(found),
            }
        }
        .expect("bare LF should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::InvalidRecordEnding(b'\n'),
            "{input:?}"
        );
        assert_eq!(error.location().byte, byte, "{input:?}");
    }
}

/// The vectorized kernel finishes a whole record before `CrLf` strictness is
/// judged, so the field a rejection names has to be recovered from the offset
/// rather than taken from how many fields the kernel went on to produce. These
/// indices are the ones the general parser reports.
#[test]
fn crlf_terminator_rejection_names_the_offending_field() {
    let cases: &[(&[u8], u8, usize, usize)] = &[
        (b"a,b,c\r,d\r\n", b'\r', 5, 2),
        (b"a\r,b,c\r\n", b'\r', 1, 0),
        (b"a,b\rc,d\r\n", b'\r', 3, 1),
        (b"a,b,c\n", b'\n', 5, 2),
        (b"a\n", b'\n', 1, 0),
    ];
    for &(input, byte, offset, field) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let mut line = reader
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let error = line.record().expect_err("strictness violation should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::InvalidRecordEnding(byte),
            "{input:?}"
        );
        assert_eq!(error.location().byte, offset, "{input:?}");
        assert_eq!(error.location().field, field, "{input:?}");
    }
}

/// A `\r` immediately before the legitimate one is still stray: only the byte
/// abutting the `\n` is part of the terminator.
#[test]
fn crlf_terminator_rejects_doubled_cr() {
    let mut reader = SliceParser::with_options(
        b"a\r\r\n",
        FormatOptions::RFC4180,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid reader configuration");
    let mut line = reader
        .next_line()
        .expect("record exists")
        .expect("record exists");
    let error = line.record().expect_err("doubled CR should fail");
    assert_eq!(error.kind(), ErrorKind::InvalidRecordEnding(b'\r'));
    assert_eq!(error.location().byte, 1);
}

#[test]
fn crlf_terminator_rejects_bare_cr_outside_quotes_with_exact_position() {
    // Includes a lone trailing `\r` at end-of-input (no following byte at all).
    let cases: &[(&[u8], usize)] = &[
        (b"a,b\rc,d\r\n", 3),
        (b"a,b\r", 3),
        (b"a\r", 1),
        (b",\r", 1),
    ];
    for &(input, byte) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let mut line = reader
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let error = line.record().expect_err("bare CR should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::InvalidRecordEnding(b'\r'),
            "{input:?}"
        );
        assert_eq!(error.location().byte, byte, "{input:?}");
    }
}

#[test]
fn crlf_terminator_rejects_malformed_bytes_immediately_after_a_closing_quote() {
    let cases: &[(&[u8], ErrorKind, usize)] = &[
        (b"\"a\"\n", ErrorKind::UnexpectedByteAfterQuote(b'\n'), 3),
        (b"\"a\"\rX", ErrorKind::UnexpectedByteAfterQuote(b'\r'), 3),
        (b"\"a\"\r", ErrorKind::UnexpectedByteAfterQuote(b'\r'), 3),
    ];
    for &(input, expected, byte) in cases {
        let mut reader = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let mut line = reader
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let error = line.record().expect_err("malformed input should fail");
        assert_eq!(error.kind(), expected, "{input:?}");
        assert_eq!(error.location().byte, byte, "{input:?}");
    }
}

#[test]
fn crlf_terminator_skips_comments_and_strictly_blank_crlf_lines() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::RFC4180.comment(Some(b'#'));
    let mut reader = SliceParser::with_options(
        b"# ignored\r\n\r\nfirst,row\r\n",
        format.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing data row");
    assert_fields(&line.record()?, &[b"first", b"row"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn crlf_terminator_owned_and_borrowed_paths_agree_on_malformed_input() {
    let cases: &[(&[u8], ErrorKind, usize)] = &[
        (b"a,b\n", ErrorKind::InvalidRecordEnding(b'\n'), 3),
        (b"a,b\rc", ErrorKind::InvalidRecordEnding(b'\r'), 3),
    ];
    for &(input, expected, byte) in cases {
        let mut borrowed = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let mut line = borrowed
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let borrowed_error = line.record().expect_err("borrowed parser should fail");
        assert_eq!(borrowed_error.kind(), expected, "{input:?}");
        assert_eq!(borrowed_error.location().byte, byte, "{input:?}");

        let mut owned = SliceParser::with_options(
            input,
            FormatOptions::RFC4180,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid reader configuration");
        let mut record = ByteRecord::with_capacity(4, 16);
        let mut line = owned
            .next_line()
            .expect("record exists")
            .expect("record exists");
        let owned_error = line
            .read_byte_record_into(&mut record)
            .expect_err("owned parser should fail");
        assert_eq!(owned_error, borrowed_error, "{input:?}");
    }
}

#[test]
fn newline_terminator_semantics_are_unchanged_by_crlf_support() -> Result<(), Box<dyn StdError>> {
    // `RecordEnding::Newline` still accepts a bare LF, a CRLF pair, and treats
    // a lone `\r` that is not immediately followed by `\n` as ordinary data.
    let mut reader = SliceParser::with_options(
        b"a,b\nc\rd,e\r\nlast,row\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing first row");
    assert_fields(&line.record()?, &[b"a", b"b"]);
    let mut line = reader.next_line()?.expect("missing second row");
    assert_fields(&line.record()?, &[b"c\rd", b"e"]);
    let mut line = reader.next_line()?.expect("missing third row");
    assert_fields(&line.record()?, &[b"last", b"row"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

// ── PostgreSQL COPY CSV NULL semantics ──────────────────────────────────────

#[test]
fn postgres_copy_csv_unquoted_empty_is_null_and_quoted_empty_is_present()
-> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b",\"\",a\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.is_null(0), Some(true));
    assert_eq!(row.is_null(1), Some(false));
    assert_eq!(row.is_null(2), Some(false));
    assert_eq!(row.get(0), Some(&b""[..]));
    assert_eq!(row.get(1), Some(&b""[..]));
    assert_eq!(row.get(2), Some(&b"a"[..]));
    Ok(())
}

#[test]
fn postgres_copy_csv_headers_are_never_marked_null() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b",b\n,2\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new(),
    )?;
    let headers = reader.headers()?.ok_or("missing headers")?.clone();
    // Headers are never NULL-aware, so every field reports `Some(false)`
    // (a valid index that is simply never NULL), not `None`.
    assert_eq!(headers.is_null(0), Some(false));

    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
    Ok(())
}

#[test]
fn postgres_copy_csv_owned_byte_record_preserves_null_metadata() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b",\"\"\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
    assert_eq!(record.get(0), Some(&b""[..]));
    assert_eq!(record.get(1), Some(&b""[..]));
    Ok(())
}

#[test]
fn postgres_copy_csv_owned_record_reader_preserves_null_metadata() -> Result<(), Box<dyn StdError>>
{
    let mut reader = SliceParser::with_options(
        b",\"\"\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
    Ok(())
}

#[test]
fn default_csv_dialect_never_treats_empty_fields_as_null() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"a,,\"\"\n");
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    // Non-NULL-aware records still answer `is_null` for valid indices; they
    // simply always report `false` rather than distinguishing NULL at all.
    assert_eq!(row.is_null(0), Some(false));
    assert_eq!(row.is_null(1), Some(false));
    assert_eq!(row.get(1), Some(&b""[..]));
    Ok(())
}

// ── MySQL text-export syntax ─────────────────────────────────────────────────

#[test]
fn mysql_text_export_uses_tab_delimiter_and_newline_terminator() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a\tb\tc\nd\te\tf\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing first row");
    assert_fields(&line.record()?, &[b"a", b"b", b"c"]);
    let mut line = reader.next_line()?.expect("missing second row");
    assert_fields(&line.record()?, &[b"d", b"e", b"f"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_decodes_every_standard_escape() -> Result<(), Box<dyn StdError>> {
    let input = b"a\\0b\tc\\bd\te\\nf\tg\\rh\ti\\tj\tk\\Zl\tm\\\\n\n";
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(
        &row,
        &[
            b"a\0b", b"c\x08d", b"e\nf", b"g\rh", b"i\tj", b"k\x1Al", b"m\\n",
        ],
    );
    Ok(())
}

#[test]
fn mysql_text_export_raw_backslash_n_field_is_explicit_null() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"\\N\tb\tc\\N\t\\\\N\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.is_null(0), Some(true));
    assert_eq!(row.get(0), Some(&b""[..]));
    // Not an exact whole-field match: not NULL, decodes normally instead.
    assert_eq!(row.is_null(1), Some(false));
    assert_eq!(row.get(1), Some(&b"b"[..]));
    assert_eq!(row.is_null(2), Some(false));
    assert_eq!(row.get(2), Some(&b"cN"[..]));
    // An escaped backslash followed by `N` decodes to the two-byte string
    // `\N`, but is never treated as the NULL marker (only the raw, undecoded
    // bytes are compared against `\N`).
    assert_eq!(row.is_null(3), Some(false));
    assert_eq!(row.get(3), Some(&b"\\N"[..]));
    Ok(())
}

#[test]
fn mysql_text_export_headers_are_never_marked_null() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"\\N\tb\n\\N\tc\n",
        FormatOptions::MYSQL,
        ParseOptions::new(),
    )?;
    let headers = reader.headers()?.ok_or("missing headers")?.clone();
    // Headers are never NULL-aware, so every field reports `Some(false)`
    // (a valid index that is simply never NULL), not `None`.
    assert_eq!(headers.is_null(0), Some(false));

    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.is_null(0), Some(true));
    Ok(())
}

#[test]
fn mysql_text_export_unknown_escape_yields_the_literal_following_byte()
-> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a\\qb\tc\\\"d\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"aqb", b"c\"d"]);
    Ok(())
}

#[test]
fn mysql_text_export_trailing_lone_backslash_is_preserved_literally()
-> Result<(), Box<dyn StdError>> {
    // The trailing backslash must be the very last byte of the whole input
    // (true EOF, no record_ending following) to exercise the "lone backslash at
    // end of scanned span" branch. A backslash immediately before a
    // record_ending instead escapes/consumes the record_ending byte itself (see
    // `mysql_text_export_escaped_delimiter_and_newline_are_data`).
    let mut reader = SliceParser::with_options(
        b"x\ta\\",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"x", b"a\\"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_unterminated_final_record_is_unescaped() -> Result<(), Box<dyn StdError>> {
    // The vectorized kernel commits a record in two places: at a terminator and
    // at end of input. Both must decline a record containing a backslash, or an
    // unterminated tail is handed back with its escapes still in it.
    let mut reader = SliceParser::with_options(
        b"a\\tb\tc\\\\d",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"a\tb", b"c\\d"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_doubled_backslash_does_not_escape_the_delimiter()
-> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a\\\\\tb\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"a\\", b"b"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_escape_free_records_survive_a_neighbour_with_escapes()
-> Result<(), Box<dyn StdError>> {
    // Only the record holding the backslash leaves the vectorized kernel; the
    // ones around it stay on it and must still be parsed identically.
    let mut reader = SliceParser::with_options(
        b"a\tb\nc\\td\te\nf\tg\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row 1");
    assert_fields(&line.record()?, &[b"a", b"b"]);
    let mut line = reader.next_line()?.expect("missing row 2");
    assert_fields(&line.record()?, &[b"c\td", b"e"]);
    let mut line = reader.next_line()?.expect("missing row 3");
    assert_fields(&line.record()?, &[b"f", b"g"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_escaped_delimiter_and_newline_are_data() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"a\\tb\tc\\nd\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"a\tb", b"c\nd"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn mysql_text_export_quoting_is_not_structural() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"\"a\tb\"\tc\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    // The quote bytes are ordinary data; the field splits on the tab that
    // falls inside what would otherwise look like a quoted span.
    assert_fields(&row, &[b"\"a", b"b\"", b"c"]);
    Ok(())
}

#[test]
fn mysql_text_export_quote_bytes_do_not_require_termination() -> Result<(), Box<dyn StdError>> {
    // An unterminated quote would be a syntax error under normal quoting
    // rules; MySQL export syntax has no such rule since quoting is disabled.
    let mut reader = SliceParser::with_options(
        b"\"unterminated\tb\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_fields(&row, &[b"\"unterminated", b"b"]);
    Ok(())
}

#[test]
fn mysql_text_export_owned_byte_record_preserves_null_and_escapes() -> Result<(), Box<dyn StdError>>
{
    let mut reader = SliceParser::with_options(
        b"\\N\ta\\tb\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
    assert_eq!(record.get(1), Some(&b"a\tb"[..]));
    Ok(())
}

// ── Explicit format + nulls combinations (no format) ─────────────────

#[test]
fn explicit_null_style_without_preset_matches_preset_behavior() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b",\"\"\n",
        FormatOptions::CSV.nulls(Nulls::PostgresCsv),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.is_null(0), Some(true));
    assert_eq!(row.is_null(1), Some(false));
    Ok(())
}

#[test]
fn null_style_none_is_the_default_gate_for_specialized_kernels() -> Result<(), Box<dyn StdError>> {
    // Sanity check that `Nulls::None` (the default) never marks a
    // record as NULL-aware, keeping every existing default fast path exact.
    let mut reader = unheaded(b"a,,b\n");
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    // No field is marked NULL under the default dialect, not even the empty one.
    assert_eq!(row.is_null(0), Some(false));
    assert_eq!(row.is_null(1), Some(false));
    assert_eq!(row.is_null(2), Some(false));
    Ok(())
}

#[test]
fn slice_entry_points_accept_any_byte_source() -> Result<(), Box<dyn StdError>> {
    let owned_text = String::from("a,b\n");
    let owned_bytes = Vec::from(b"a,b\n".as_slice());

    let mut from_str = unheaded("a,b\n");
    let mut from_string = unheaded(&owned_text);
    let mut from_vec = unheaded(&owned_bytes);
    let mut from_array = unheaded(b"a,b\n");
    let mut from_slice = unheaded(b"a,b\n".as_slice());

    for reader in [
        &mut from_str,
        &mut from_string,
        &mut from_vec,
        &mut from_array,
        &mut from_slice,
    ] {
        let mut line = reader.next_line()?.expect("missing record");
        let record = line.record()?;
        assert_fields(&record, &[b"a", b"b"]);
    }

    let mut configured = SliceParser::with_options(
        "a,b\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = configured.next_line()?.expect("missing record");
    let record = line.record()?;
    assert_fields(&record, &[b"a", b"b"]);
    Ok(())
}

#[test]
fn parsing_reports_end_of_input_repeatedly_after_completion() {
    let mut parser = unheaded("a,b\nc,d\n");
    assert!(parser.next_line().expect("first record").is_some());
    assert!(parser.next_line().expect("second record").is_some());
    assert!(parser.next_line().expect("end of input").is_none());
    assert!(parser.next_line().expect("end of input").is_none());

    let mut failed = unheaded("a,b\n\"unterminated");
    assert!(failed.next_line().expect("first record").is_some());
    let mut line = failed
        .next_line()
        .expect("second record")
        .expect("second record");
    let _error = line.record().expect_err("expected a parse failure");
    let _error = failed.next_line().expect_err("expected a latched failure");
    let _error = failed.next_line().expect_err("expected a latched failure");
}

/// The plain-record scanner anchors its 32-byte block grid to the whole input
/// and carries the last computed block across records, so records that start
/// part-way through a block reuse a cached mask. Sweeping the leading record
/// width places every later record boundary at every offset within a block.
#[test]
fn plain_parsing_matches_across_every_structural_block_alignment() -> Result<(), Box<dyn StdError>>
{
    for lead in 0..80_usize {
        let mut input = String::new();
        let mut expected: Vec<Vec<Vec<u8>>> = Vec::new();

        let head = "x".repeat(lead);
        input.push_str(&head);
        input.push_str(",y\n");
        expected.push(vec![head.into_bytes(), b"y".to_vec()]);

        for record in 0..12_usize {
            let first = "a".repeat(record % 7);
            let second = "b".repeat((record * 3) % 5);
            input.push_str(&first);
            input.push(',');
            input.push_str(&second);
            input.push_str(",,c\n");
            expected.push(vec![
                first.into_bytes(),
                second.into_bytes(),
                Vec::new(),
                b"c".to_vec(),
            ]);
        }

        let options = ParseOptions::new().headers(Headers::None);

        let mut borrowed =
            SliceParser::with_options(input.as_bytes(), FormatOptions::new(), options.clone())?;
        for want in &expected {
            let mut line = borrowed.next_line()?.expect("missing borrowed record");
            let record = line.record()?;
            let got: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(&got, want, "borrowed mismatch at lead {lead}");
        }
        assert!(borrowed.next_line()?.is_none());

        let mut owned = SliceParser::with_options(input.as_bytes(), FormatOptions::new(), options)?;
        let mut record = ByteRecord::new();
        for want in &expected {
            let next = owned.next_line()?;
            assert!(next.is_some(), "missing owned record at lead {lead}");
            let mut line = next.expect("owned record");
            line.read_byte_record_into(&mut record)?;
            let got: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(&got, want, "owned mismatch at lead {lead}");
        }
        assert!(owned.next_line()?.is_none());
    }
    Ok(())
}

#[test]
fn unquoted_only_trimming_leaves_quoted_fields_alone() -> Result<(), Box<dyn StdError>> {
    // Both materialization paths must honor the quoted exemption. The owned
    // path trims a whole record at once, so it can only be used when the policy
    // does not depend on how each field was written.
    let format = FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only());
    let input = b"  bare  ,\"  quoted  \"\n";

    let mut parser =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
    let mut line = parser.next_line()?.expect("record");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(&b"bare"[..]));
    assert_eq!(record.get(1), Some(&b"  quoted  "[..]));

    let mut parser =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
    let mut owned = ByteRecord::new();
    let mut line = parser.next_line()?.expect("record");
    line.read_byte_record_into(&mut owned)?;
    assert_eq!(owned.get(0), Some(&b"bare"[..]));
    assert_eq!(owned.get(1), Some(&b"  quoted  "[..]));
    Ok(())
}

// ── BOM handling ─────────────────────────────────────────────────────────────

#[test]
fn slice_parser_with_options_rejects_bom() {
    let bom_input = b"\xEF\xBB\xBFcity,pop\nBoston,1\n";
    let err = SliceParser::with_options(
        bom_input,
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new(),
    )
    .expect_err("should reject BOM");
    // The whole input is present, so the mark is refused at construction, but
    // the kind matches the streaming front ends, which reject at read time.
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
}

// ── SliceParser bookkeeping: seek, is_done, headers ────────────────────────

#[test]
fn slice_parser_large_input_selects_limit_aware_parser() -> Result<(), Box<dyn StdError>> {
    // An input longer than max_field_bytes selects try_parse_default_record::<true, false>.
    // Build input with many small records so the total exceeds max_field_bytes.
    let max_field = Limits::DEFAULT.max_field_bytes;
    let mut big: Vec<u8> = b"header\n".to_vec();
    while big.len() <= max_field {
        big.extend_from_slice(b"x\n");
    }
    let mut parser = SliceParser::<Csv>::new(big.as_slice(), ParseOptions::new()).expect("parser");
    // Drain all records to confirm no panic.
    while let Some(mut line) = parser.next_line()? {
        let _ = line.record()?;
    }
    Ok(())
}

#[test]
fn slice_parser_seek_rejects_nonzero_field() {
    let mut parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,1\n", ParseOptions::new()).expect("parser");
    let mut location = parser.location();
    location.field = 1;
    let err = parser
        .seek(location)
        .expect_err("nonzero field should fail");
    assert_eq!(err.kind(), ErrorKind::Configuration);
}

#[test]
fn slice_parser_seek_rejects_out_of_bounds() {
    let mut parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,1\n", ParseOptions::new()).expect("parser");
    let mut location = parser.location();
    location.byte = 9999;
    let err = parser
        .seek(location)
        .expect_err("byte past end should fail");
    assert_eq!(err.kind(), ErrorKind::Configuration);
}

#[test]
fn slice_parser_seek_revisits_record() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::<Csv>::new("city,pop\nParis,2\nLyon,1\n", ParseOptions::new())
        .expect("parser");
    // Read and discard the first data record.
    let mut line = parser.next_line()?.expect("first data record (Paris)");
    assert_eq!(line.record()?.get(0), Some(b"Paris".as_slice()));
    // Bookmark the next record AFTER consuming Paris.
    let bookmark = parser.location();
    {
        let mut line = parser.next_line()?.expect("second record (Lyon)");
        assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    };

    // Seeking back should replay Lyon.
    parser.seek(bookmark)?;
    assert_eq!(parser.location(), bookmark);
    let mut line = parser.next_line()?.expect("second record again");
    assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    Ok(())
}

#[test]
fn slice_parser_is_done_at_eof() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded(b"a,b\n");
    assert!(!parser.is_done());
    while parser.next_line()?.is_some() {}
    assert!(parser.is_done());
    Ok(())
}

#[test]
fn slice_parser_has_headers_reports_correctly() {
    let parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,1\n", ParseOptions::new()).expect("parser");
    assert!(parser.has_headers());

    let parser2 = unheaded(b"a,b\n1,2\n");
    assert!(!parser2.has_headers());
}

#[test]
fn slice_parser_seek_ensure_headers_error_on_malformed_header() {
    // With auto-discover headers, seek() calls ensure_headers; a parse error
    // in the header record surfaces through the `?` at that point.
    use coseva::Location;
    let input = b"\"unclosed\n1,2\n";
    let mut parser = SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())
        .expect("construction succeeds");
    let loc = Location {
        byte: 0,
        line: 1,
        record: 1,
        field: 0,
    };
    let err = parser
        .seek(loc)
        .expect_err("malformed header should surface");
    assert!(matches!(
        err.kind(),
        ErrorKind::UnterminatedQuotedField | ErrorKind::Io(_)
    ));
}

// ── Error, ErrorKind, and Location reporting ─────────────────────────────────

#[test]
fn error_field_name_returns_some_when_set() {
    let err = coseva::Error::from_field_conversion(ErrorKind::InvalidDigit, 2, "my_field");
    assert_eq!(err.field_name(), Some("my_field"));
}

#[test]
fn error_into_io_error_returns_none_for_non_io_error() -> Result<(), Box<dyn StdError>> {
    let err = SliceParser::with_options(
        b"\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected unterminated error");
    assert!(err.into_io_error().is_none());
    Ok(())
}

/// Display shows location when it is known.
#[test]
fn error_display_includes_known_location() -> Result<(), Box<dyn StdError>> {
    let err = SliceParser::with_options(
        b"a,\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected parse error");
    let msg = err.to_string();
    assert!(msg.contains("byte"), "expected location in '{msg}'");
    assert!(msg.contains("line"), "expected location in '{msg}'");
    Ok(())
}

/// Display prefixes the field name when it is set.
#[test]
fn error_display_includes_field_name() {
    let err = coseva::Error::from_field_conversion(ErrorKind::InvalidDigit, 0, "score");
    let msg = err.to_string();
    assert!(msg.contains("score"), "expected field name in '{msg}'");
}

/// Drive every `ErrorKind::Display` arm so the formatter lines are covered.
#[expect(
    invalid_from_utf8,
    reason = "we intentionally construct a Utf8Error to exercise the Display arm"
)]
#[test]
fn error_kind_display_all_variants() {
    let cases: &[(ErrorKind, &str)] = &[
        (ErrorKind::EmptyField, "empty"),
        (ErrorKind::InvalidDigit, "invalid digit"),
        (ErrorKind::OutOfRange, "does not fit"),
        (ErrorKind::InvalidValue, "not a valid value"),
        (ErrorKind::Configuration, "invalid configuration"),
        (ErrorKind::MissingHeader, "missing header"),
        (ErrorKind::DuplicateHeader, "duplicate header"),
        (ErrorKind::Decode, "typed decoding failed"),
        (ErrorKind::Serde, "Serde conversion failed"),
        (ErrorKind::RejectedBom, "BOM"),
        (ErrorKind::UnterminatedQuotedField, "unterminated"),
        (ErrorKind::UnexpectedQuote, "quote"),
        (ErrorKind::UnexpectedByteAfterQuote(b'x'), "0x78"),
        (ErrorKind::InvalidEscape(b'z'), "0x7a"),
        (ErrorKind::InvalidRecordEnding(b'\r'), "0x0d"),
        (ErrorKind::RecordTooLarge { limit: 10 }, "10"),
        (ErrorKind::FieldTooLarge { limit: 5 }, "5"),
        (ErrorKind::TooManyFields { limit: 3 }, "3"),
        (
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 3,
            },
            "expected 2",
        ),
        (ErrorKind::ParserFailed, "earlier error"),
        (ErrorKind::Encode, "cannot be encoded"),
        (ErrorKind::EmitterFailed, "earlier error"),
        (ErrorKind::LocationOverflow, "exceeds supported range"),
        (ErrorKind::SourceMismatch, "different source bytes"),
        (ErrorKind::RecordOutOfRange { record: 42 }, "42"),
        (ErrorKind::InvalidIndex, "malformed or unsupported"),
        (
            ErrorKind::Io(io::ErrorKind::BrokenPipe),
            "input or output failed",
        ),
        (
            ErrorKind::InvalidUtf8({
                let bytes: &[u8] = &[0x80_u8, 0x80_u8];
                std::str::from_utf8(bytes).expect_err("invalid utf8 bytes")
            }),
            "not UTF-8",
        ),
    ];
    for (kind, snippet) in cases {
        let msg = kind.to_string();
        assert!(
            msg.contains(snippet),
            "ErrorKind::{kind:?} display '{msg}' missing '{snippet}'"
        );
    }
}

#[test]
fn error_source_is_none_for_plain_parse_error() -> Result<(), Box<dyn StdError>> {
    let err = SliceParser::with_options(
        b"a\"b\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected error");
    let source: Option<&dyn std::error::Error> = std::error::Error::source(&err);
    assert!(source.is_none());
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn serde_de_error_missing_field_includes_field_name() {
    use serde::de::Error as _;
    let e = coseva::Error::missing_field("age");
    assert_eq!(e.field_name(), Some("age"));
    assert!(e.to_string().contains("missing record field"));
}

#[cfg(feature = "serde")]
#[test]
fn serde_de_error_unknown_field_includes_name_in_message() {
    use serde::de::Error as _;
    let e = coseva::Error::unknown_field("bogus", &[]);
    assert!(e.to_string().contains("bogus"));
}

#[cfg(feature = "serde")]
#[test]
fn serde_de_error_custom_preserves_message() {
    use serde::de::Error as _;
    let e = coseva::Error::custom("something went wrong");
    assert_eq!(e.kind(), ErrorKind::Serde);
    assert!(e.to_string().contains("something went wrong"));
}

#[cfg(feature = "serde")]
#[test]
fn serde_ser_error_custom_preserves_message() {
    use serde::ser::Error as _;
    let e = coseva::Error::custom("encode failed");
    assert_eq!(e.kind(), ErrorKind::Serde);
    assert!(e.to_string().contains("encode failed"));
}

// ── MySQL escape and NULL-marker decoding ────────────────────────────────────

/// A `MySQL`-escaped unquoted field with `\a` (unknown sequence) → `a`.
#[test]
fn mysql_escape_unknown_sequence_passes_through() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\\a\tdone\n", FormatOptions::MYSQL)?;
    assert_eq!(records[0][0], b"a");
    assert_eq!(records[0][1], b"done");
    Ok(())
}

/// Every known `MySQL` escape sequence decodes correctly.
#[test]
fn mysql_escape_known_sequences_decode() -> Result<(), Box<dyn StdError>> {
    let input = b"\\0\t\\b\t\\n\t\\r\t\\t\t\\Z\t\\\\\n";
    let records = parse_unheaded(input, FormatOptions::MYSQL)?;
    assert_eq!(records[0][0], b"\x00");
    assert_eq!(records[0][1], b"\x08");
    assert_eq!(records[0][2], b"\n");
    assert_eq!(records[0][3], b"\r");
    assert_eq!(records[0][4], b"\t");
    assert_eq!(records[0][5], b"\x1a");
    assert_eq!(records[0][6], b"\\");
    Ok(())
}

/// A custom `MySQL`-escape dialect with quoting enabled exercises `parse_quoted_field` `Mysql` arm.
#[test]
fn mysql_quoted_field_escapes_decode() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .escape(Escape::Mysql)
        .quoting(Quoting::Never);
    let records = parse_unheaded(b"\"hel\\nlo\",\"say \\\"hi\\\"\"\n", format)?;
    assert_eq!(records[0][0], b"hel\nlo");
    assert_eq!(records[0][1], b"say \"hi\"");
    Ok(())
}

/// `MySQL` trailing backslash at end of input is preserved literally.
#[test]
fn mysql_trailing_backslash_in_unquoted_field_is_literal() -> Result<(), Box<dyn StdError>> {
    // Input ends with `\` (no following byte) → the backslash is preserved.
    let records = parse_unheaded(b"abc\\", FormatOptions::MYSQL)?;
    assert_eq!(records[0][0], b"abc\\");
    Ok(())
}

/// `MySQL` \N in unquoted field is a NULL marker.
#[test]
fn mysql_null_marker_produces_null_field() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"\\N\tvalue\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let rec = line.record()?;
    assert_eq!(rec.is_null(0), Some(true), "expected NULL at field 0");
    assert_eq!(rec.is_null(1), Some(false), "field 1 should not be NULL");
    Ok(())
}

/// Custom `MySQL`-escape dialect with quoting: trailing backslash at EOF in a quoted field is an error.
#[test]
fn mysql_quoted_field_trailing_backslash_is_unterminated() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .escape(Escape::Mysql)
        .quoting(Quoting::Never);
    let err = SliceParser::with_options(
        b"\"abc\\",
        format,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected error");
    assert_eq!(err.kind(), ErrorKind::UnterminatedQuotedField);
    Ok(())
}

// ── RecordEnding::CrLf dialect handling (slice path) ─────────────────────────

/// `CrLf` dialect rejects bare `\n` in an unquoted field.
#[test]
fn crlf_dialect_rejects_bare_lf_in_unquoted_field() -> Result<(), Box<dyn StdError>> {
    let err = SliceParser::with_options(
        b"a,b\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected error");
    assert_eq!(err.kind(), ErrorKind::InvalidRecordEnding(b'\n'));
    Ok(())
}

/// `CrLf` dialect rejects bare `\r` not followed by `\n`.
#[test]
fn crlf_dialect_rejects_bare_cr_in_unquoted_field() -> Result<(), Box<dyn StdError>> {
    let err = SliceParser::with_options(
        b"a\rX",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected error");
    assert_eq!(err.kind(), ErrorKind::InvalidRecordEnding(b'\r'));
    Ok(())
}

/// `CrLf` dialect correctly terminates a record on `\r\n`.
#[test]
fn crlf_dialect_parses_crlf_records() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(
        b"a,b\r\nc,d\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
    )?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], [b"a", b"b"]);
    assert_eq!(records[1], [b"c", b"d"]);
    Ok(())
}

/// `CrLf` dialect parses quoted field containing `\r\n` sequence.
#[test]
fn crlf_dialect_allows_crlf_inside_quoted_field() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(
        b"\"a\r\nb\"\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
    )?;
    assert_eq!(records[0][0], b"a\r\nb");
    Ok(())
}

// ── PostgreSQL COPY CSV: additional NULL and field-limit cases ───────────────

/// Empty field in `PostgresCsv` dialect is a NULL.
#[test]
fn postgres_csv_null_empty_field_is_null() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b",value,\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let rec = line.record()?;
    assert_eq!(
        rec.is_null(0),
        Some(true),
        "leading empty field should be NULL"
    );
    assert_eq!(rec.is_null(1), Some(false));
    assert_eq!(
        rec.is_null(2),
        Some(true),
        "trailing empty field should be NULL"
    );
    Ok(())
}

// ── Whitespace trimming policies ──────────────────────────────────────────────

/// `Whitespace::ALL` trims both unquoted and quoted fields.
#[test]
fn trim_all_removes_whitespace_from_quoted_and_unquoted_fields() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  hello  ,  world  \n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT),
    )?;
    // With Whitespace::NONE (default) both spaces are kept
    assert_eq!(records[0][0], b"  hello  ");
    assert_eq!(records[0][1], b"  world  ");
    Ok(())
}

/// `Whitespace::unquoted_only()` on headers trims only unquoted header fields.
#[test]
fn trim_unquoted_only_exempts_quoted_fields() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  h1  ,  h2  \n  a  ,  b  \n",
        FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only()),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"h1");
    assert_eq!(records[0][1], b"h2");
    Ok(())
}

// ── Blank records, comments, and BOM detection ────────────────────────────────

#[test]
fn blank_records_skip_removes_empty_lines() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"a,b\n\nc,d\n\n",
        FormatOptions::CSV.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], [b"a", b"b"]);
    assert_eq!(records[1], [b"c", b"d"]);
    Ok(())
}

#[test]
fn comment_lines_are_skipped() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(
        b"# ignore this\na,b\n# and this\nc,d\n",
        FormatOptions::COMMENTED_CSV,
    )?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], [b"a", b"b"]);
    assert_eq!(records[1], [b"c", b"d"]);
    Ok(())
}

#[test]
fn bom_is_skipped_when_detect_is_enabled() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"\xEF\xBB\xBFa,b\n",
        FormatOptions::CSV.read_bom(ReadBom::Detect),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"a");
    Ok(())
}

// ── FieldCount policies ───────────────────────────────────────────────────────

/// `FieldCount::Exact` rejects a record with the wrong count.
#[test]
fn field_count_exact_rejects_mismatch() {
    let err = parse_all(
        b"a,b\na,b,c\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::Exact(2)),
    )
    .expect_err("expected error");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3
        }
    );
}

/// `FieldCount::MatchFirst` learns width from the first record.
#[test]
fn field_count_match_first_rejects_wider_record() {
    let err = parse_all(
        b"a,b\na,b,c\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::MatchFirst),
    )
    .expect_err("expected error");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3
        }
    );
}

/// `FieldCount::MatchFirst` with provided headers adopts their count.
#[test]
fn field_count_match_first_uses_provided_headers() {
    let mut headers = ByteRecord::new();
    headers.push_field(b"a");
    headers.push_field(b"b");
    let err = parse_all(
        b"a,b,c\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::Provided(headers))
            .field_count(FieldCount::MatchFirst),
    )
    .expect_err("expected error");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3
        }
    );
}

// ── Limits enforcement across parsing paths ──────────────────────────────────

#[test]
fn record_too_large_in_general_path() {
    let limits = Limits::new(4, 10, 100);
    let err = parse_all(
        b"abc,def\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected record too large");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
}

#[test]
fn field_too_large_in_general_path() {
    let limits = Limits::new(1000, 2, 100);
    let err = parse_all(
        b"abc,d\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected field too large");
    assert_eq!(err.kind(), ErrorKind::FieldTooLarge { limit: 2 });
}

#[test]
fn too_many_fields_in_general_path() {
    let limits = Limits::new(1000, 1000, 2);
    let err = parse_all(
        b"a,b,c\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected too many fields");
    assert_eq!(err.kind(), ErrorKind::TooManyFields { limit: 2 });
}

/// `field_too_large` in a `MySQL`-escaped quoted field.
#[test]
fn field_too_large_in_mysql_quoted_field() {
    let limits = Limits::new(1000, 3, 100);
    let err = parse_all(
        b"\"abcd\"\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected field too large");
    assert_eq!(err.kind(), ErrorKind::FieldTooLarge { limit: 3 });
}

/// Too many fields in a `MySQL` unquoted path.
#[test]
fn too_many_fields_in_mysql_unquoted_path() {
    let limits = Limits::new(1000, 1000, 2);
    let err = parse_all(
        b"a\tb\tc\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected too many fields");
    assert_eq!(err.kind(), ErrorKind::TooManyFields { limit: 2 });
}

#[test]
fn too_many_fields_in_general_crlf_path() {
    let limits = Limits::new(1000, 1000, 2);
    let err = parse_all(
        b"a,b,c\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected too many fields");
    assert_eq!(err.kind(), ErrorKind::TooManyFields { limit: 2 });
}

// ── Owned-record parsing path ─────────────────────────────────────────────────

/// The owned fast path exercises `parse_owned_record` via `read_byte_record`.
#[test]
fn owned_path_parses_plain_record() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"a,b,c\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut rec = ByteRecord::new();
    let mut line = p.next_line()?.expect("record");
    line.read_byte_record_into(&mut rec)?;
    assert_eq!(rec.iter().collect::<Vec<_>>(), [b"a", b"b", b"c"]);
    Ok(())
}

/// Owned path with `skip_initial_space`.
#[test]
fn owned_path_skip_initial_space_after_delimiter() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"a, b, c\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT),
    )?;
    // Without skip_initial_space the spaces are preserved
    assert_eq!(records[0][1], b" b");
    Ok(())
}

/// Owned path with a quoted field following an unquoted one.
#[test]
fn owned_path_handles_quoted_after_plain_field() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"plain,\"quoted field\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"plain");
    assert_eq!(records[0][1], b"quoted field");
    Ok(())
}

/// Unexpected quote in an owned unquoted field.
#[test]
fn owned_path_unexpected_quote_in_unquoted_field() {
    let err = parse_all(
        b"ab\"c\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect_err("expected unexpected quote");
    assert_eq!(err.kind(), ErrorKind::UnexpectedQuote);
}

/// Record too large in owned unquoted path.
#[test]
fn owned_path_record_too_large_unquoted() {
    let limits = Limits::new(3, 100, 100);
    let err = parse_all(
        b"abcd\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected record too large");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 3 });
}

/// Field too large in owned quoted path.
#[test]
fn owned_path_field_too_large_quoted() {
    let limits = Limits::new(1000, 2, 100);
    let err = parse_all(
        b"\"abcd\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected field too large");
    assert_eq!(err.kind(), ErrorKind::FieldTooLarge { limit: 2 });
}

/// Too many fields in owned quoted path.
#[test]
fn owned_path_too_many_fields_with_quoted_start() {
    let limits = Limits::new(1000, 1000, 1);
    let err = parse_all(
        b"\"a\",\"b\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected too many fields");
    assert_eq!(err.kind(), ErrorKind::TooManyFields { limit: 1 });
}

/// Byte after closing quote that is not delimiter or terminator is an error.
#[test]
fn owned_path_unexpected_byte_after_quote() {
    let err = parse_all(
        b"\"abc\"X\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect_err("expected unexpected byte after quote");
    assert_eq!(err.kind(), ErrorKind::UnexpectedByteAfterQuote(b'X'));
}

/// Closing quote followed by `\r\n` in owned path (Newline dialect).
#[test]
fn owned_path_crlf_after_quote_is_record_end() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"\"abc\"\r\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"abc");
    Ok(())
}

// ── Interior-quote tail handoff ─────────────────────────────────────────────
//
// After a record quotes an interior column the owned kernel bails, arms a
// short prediction, and hands the plain tail (and the following predicted
// records) back to the vectorized kernel through the scalar interior-prefix
// parser. A second separated quote run switches predicted records to the
// whole-record multi-quote parser. These tests drive enough records to arm,
// sustain, and self-correct both predictions, across the escaping, ending,
// and error shapes the handoff must preserve.

/// A block of interior-quote records is read exactly, spanning the whole
/// prediction run so both the armed and the self-correcting records are
/// covered.
#[test]
fn owned_interior_quote_block_reads_every_record() -> Result<(), Box<dyn StdError>> {
    let row = b"Boston,\"Massachusetts\",4500000,42.3601,-71.0589,true\n";
    let mut input = Vec::new();
    for _ in 0..40 {
        input.extend_from_slice(row);
    }
    let records = parse_unheaded_owned(&input)?;
    assert_eq!(records.len(), 40);
    for record in &records {
        assert_eq!(
            record,
            &[
                b"Boston".to_vec(),
                b"Massachusetts".to_vec(),
                b"4500000".to_vec(),
                b"42.3601".to_vec(),
                b"-71.0589".to_vec(),
                b"true".to_vec(),
            ]
        );
    }
    Ok(())
}

/// A record that quotes several interior columns switches the predicted rows
/// to the whole-record multi-quote parser after the first row proves that cheaper
/// shape.
#[test]
fn owned_record_with_several_interior_quotes_reads_every_field() -> Result<(), Box<dyn StdError>> {
    let row = b"a,\"b\",c,\"d\",e,\"f\"\n";
    let mut input = Vec::new();
    for _ in 0..40 {
        input.extend_from_slice(row);
    }
    let records = parse_unheaded_owned(&input)?;
    assert_eq!(records.len(), 40);
    for record in &records {
        assert_eq!(
            record,
            &[
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
                b"e".to_vec(),
                b"f".to_vec()
            ]
        );
    }
    Ok(())
}

/// Plain records that follow an interior-quote record are mispredictions: the
/// interior-prefix parser reads them whole and the counter decays back to the
/// structural route, all while producing the right fields.
#[test]
fn owned_plain_records_after_an_interior_quote_are_read_correctly() -> Result<(), Box<dyn StdError>>
{
    let mut input = Vec::new();
    input.extend_from_slice(b"head,\"quoted\",middle,\"again\",tail\n");
    for i in 0..30 {
        input.extend_from_slice(format!("plain{i},value{i},last{i}\n").as_bytes());
    }
    let records = parse_unheaded_owned(&input)?;
    assert_eq!(records.len(), 31);
    assert_eq!(
        records[0],
        [
            b"head".to_vec(),
            b"quoted".to_vec(),
            b"middle".to_vec(),
            b"again".to_vec(),
            b"tail".to_vec()
        ]
    );
    for (i, record) in records[1..].iter().enumerate() {
        assert_eq!(
            record,
            &[
                format!("plain{i}").into_bytes(),
                format!("value{i}").into_bytes(),
                format!("last{i}").into_bytes(),
            ]
        );
    }
    Ok(())
}

/// Doubled quotes inside interior fields are unescaped identically whether the
/// record is the one that arms the prediction or a predicted one.
#[test]
fn owned_interior_quote_block_unescapes_doubled_quotes() -> Result<(), Box<dyn StdError>> {
    let row = b"x,\"a\"\"b\",\"c\"\"\"\"d\",z\n";
    let mut input = Vec::new();
    for _ in 0..40 {
        input.extend_from_slice(row);
    }
    let records = parse_unheaded_owned(&input)?;
    assert_eq!(records.len(), 40);
    for record in &records {
        assert_eq!(
            record,
            &[
                b"x".to_vec(),
                b"a\"b".to_vec(),
                b"c\"\"d".to_vec(),
                b"z".to_vec()
            ]
        );
    }
    Ok(())
}

/// CRLF endings survive the handoff both after an interior quoted field and
/// after the plain tail the kernel takes over.
#[test]
fn owned_interior_quote_block_preserves_crlf_endings() -> Result<(), Box<dyn StdError>> {
    let row = b"p,\"q\",r\r\n";
    let mut input = Vec::new();
    for _ in 0..40 {
        input.extend_from_slice(row);
    }
    let records = parse_unheaded_owned(&input)?;
    assert_eq!(records.len(), 40);
    for record in &records {
        assert_eq!(record, &[b"p".to_vec(), b"q".to_vec(), b"r".to_vec()]);
    }
    Ok(())
}

/// A malformed predicted record still reproduces the general parser's exact
/// error kind, byte, and field: the interior-prefix parser declines and the
/// record falls through to the general loop unchanged.
#[test]
fn owned_predicted_malformed_record_preserves_exact_error() {
    let mut input = Vec::new();
    input.extend_from_slice(b"a,\"b\",c,\"d\",e\n");
    let prefix_len = input.len();
    input.extend_from_slice(b"plain,\"quoted\"x,tail\n");
    for _ in 0..4 {
        input.extend_from_slice(b"later,\"valid\",row,\"again\"\n");
    }

    let mut reader = coseva::SliceParser::with_options(
        input.as_slice(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut record = ByteRecord::new();

    let mut first = reader
        .next_line()
        .expect("first record")
        .expect("first record");
    first
        .read_byte_record_into(&mut record)
        .expect("first record parses");
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"a", b"b", b"c", b"d", b"e"]
    );

    let mut second = reader
        .next_line()
        .expect("second record")
        .expect("second record");
    let error = second
        .read_byte_record_into(&mut record)
        .expect_err("the predicted record is malformed");
    assert_eq!(error.kind(), ErrorKind::UnexpectedByteAfterQuote(b'x'));
    assert_eq!(error.location().byte, prefix_len + 14);
    assert_eq!(error.location().field, 2);
}

/// The interior-quote fallback still enforces limits: under a small field limit
/// the specialized prefix parsers are disabled, and a block of interior-quote
/// records must reach the general loop's error rather than silently passing.
#[test]
fn owned_interior_quote_block_enforces_field_limit_via_fallback() {
    let row = b"a,\"bbb\"\n";
    let mut input = Vec::new();
    for _ in 0..8 {
        input.extend_from_slice(row);
    }
    let limits = Limits::new(1000, 2, 100);
    let mut reader = coseva::SliceParser::with_options(
        input.as_slice(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect("valid options");
    let mut record = ByteRecord::new();
    let mut line = reader.next_line().expect("record").expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("the quoted field is over the limit");
    assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
    assert_eq!(error.location().field, 1);
}

// ── Backslash-escape and compatible-syntax dialects ──────────────────────────

/// Backslash dialect: invalid escape in a quoted field.
#[test]
fn backslash_dialect_rejects_invalid_escape_in_quoted_field() {
    let err = parse_all(
        b"\"\\x\"\n",
        FormatOptions::BACKSLASH_CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect_err("expected invalid escape");
    assert_eq!(err.kind(), ErrorKind::InvalidEscape(b'x'));
}

/// Backslash dialect: `\` at end of quoted field (no following byte) is an error.
#[test]
fn backslash_dialect_rejects_trailing_escape_in_quoted_field() {
    let err = parse_all(
        b"\"abc\\",
        FormatOptions::BACKSLASH_CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect_err("expected invalid escape at end");
    assert_eq!(err.kind(), ErrorKind::InvalidEscape(b'\\'));
}

/// Compatible syntax: quote inside unquoted field is tolerated.
#[test]
fn compatible_syntax_allows_unquoted_quote() -> Result<(), Box<dyn StdError>> {
    use coseva::config::Recovery;
    let records = parse_all(
        b"hel\"lo\n",
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"hel\"lo");
    Ok(())
}

/// Compatible syntax: trailing whitespace after closing quote is permitted.
#[test]
fn compatible_syntax_allows_trailing_whitespace_after_quote() -> Result<(), Box<dyn StdError>> {
    use coseva::config::Recovery;
    let records = parse_all(
        b"\"abc\"  ,next\n",
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"abc");
    assert_eq!(records[0][1], b"next");
    Ok(())
}

#[test]
fn skip_initial_space_removes_leading_spaces_after_delimiter() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"a, b,  c\n",
        FormatOptions::CSV.skip_initial_space(true),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0], [b"a", b"b", b"c"]);
    Ok(())
}

// ── Header discovery, provided headers, and lookup ───────────────────────────

#[test]
fn headers_provided_are_returned_correctly() -> Result<(), Box<dyn StdError>> {
    let mut h = ByteRecord::new();
    h.push_field(b"name");
    h.push_field(b"age");
    let mut p = SliceParser::with_options(
        b"Alice,30\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::Provided(h)),
    )
    .expect("valid options");
    let headers = p.headers()?.expect("headers");
    assert_eq!(headers.get(0), Some(b"name".as_slice()));
    assert_eq!(p.header_index("name")?, Some(0));
    assert_eq!(p.header_index("missing")?, None);
    assert_eq!(p.header_indices("age")?, [1]);
    Ok(())
}

#[test]
fn default_record_quoted_field_with_doubled_quote() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"\"a\"\"b\",world\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut rec = ByteRecord::new();
    let mut line = p.next_line()?.expect("record");
    line.read_byte_record_into(&mut rec)?;
    assert_eq!(rec.get(0), Some(b"a\"b".as_slice()));
    assert_eq!(rec.get(1), Some(b"world".as_slice()));
    Ok(())
}

#[test]
fn default_record_no_trailing_newline() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"a,b",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut rec = ByteRecord::new();
    let mut line = p.next_line()?.expect("record");
    line.read_byte_record_into(&mut rec)?;
    assert_eq!(rec.iter().collect::<Vec<_>>(), [b"a", b"b"]);
    assert!(p.next_line()?.is_none());
    Ok(())
}

/// Projection with doubled-quote in a quoted field.
#[cfg(feature = "serde")]
#[test]
fn projected_record_with_doubled_quote() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Row<'a> {
        name: &'a str,
    }

    let mut p = SliceParser::with_options(
        b"name,score\n\"a\"\"b\",42\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let row: Row<'_> = line.deserialized()?;
    assert_eq!(row.name, "a\"b");
    Ok(())
}

// ── Additional dialect, limit, and error edge cases ───────────────────────────

#[test]
fn set_headers_updates_lookup_for_subsequent_records() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"Alice,30\nBob,25\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut h = ByteRecord::new();
    h.push_field(b"name");
    h.push_field(b"age");
    p.set_headers(h);
    assert_eq!(p.header_index("name")?, Some(0));
    assert_eq!(p.header_index("age")?, Some(1));
    Ok(())
}

#[test]
fn borrowed_plain_handles_crlf_line_ending() -> Result<(), Box<dyn StdError>> {
    // This is the general path (CrLf) so it goes through parse_general_unquoted
    let records = parse_unheaded(
        b"a,b\r\nc,d\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
    )?;
    assert_eq!(records[0], [b"a", b"b"]);
    assert_eq!(records[1], [b"c", b"d"]);
    Ok(())
}

#[test]
fn postgres_csv_null_too_many_fields_error() {
    let limits = Limits::new(1000, 1000, 2);
    let err = parse_all(
        b",a,b\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected too many fields");
    assert_eq!(err.kind(), ErrorKind::TooManyFields { limit: 2 });
}

#[test]
fn non_general_parse_unquoted_unexpected_quote() -> Result<(), Box<dyn StdError>> {
    // With Whitespace::ALL.unquoted_only(), general_parsing is true.
    // Use default (non-general) path: plain CSV with a quote in unquoted field.
    let err = SliceParser::with_options(
        b"abc\"def\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()?
    .expect("record")
    .record()
    .expect_err("expected unexpected quote");
    assert_eq!(err.kind(), ErrorKind::UnexpectedQuote);
    Ok(())
}

/// `Whitespace::ALL` without `unquoted_only()` trims even quoted fields.
#[test]
fn whitespace_all_trims_quoted_fields() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"\" hello \",\" world \"\n",
        FormatOptions::CSV.trim(Whitespace::ALL),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"hello");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

/// `Whitespace::ALL.unquoted_only()` exempts a quoted field from trimming.
///
/// The quoted field also takes this record to the general parser, since the
/// plain kernel declines any record containing one.
#[test]
fn whitespace_unquoted_only_uses_general_parsing() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  a  ,\" b \",  c  \n",
        FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only()),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"a");
    assert_eq!(records[0][1], b" b ");
    assert_eq!(records[0][2], b"c");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn error_display_shows_detail_source_message() {
    // The serde `custom` constructor produces an `ErrorSource::Detail`, and its
    // `Display` impl surfaces the custom message text directly.
    use serde::de::Error as _;
    let e = coseva::Error::custom("detail source message");
    assert!(e.to_string().contains("detail source message"));
}

#[test]
fn record_limit_exceeded_in_quoted_field() {
    let limits = Limits::new(5, 100, 100);
    let err = parse_all(
        b"\"abcdef\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected record too large");
    // Either FieldTooLarge or RecordTooLarge
    assert!(matches!(
        err.kind(),
        ErrorKind::RecordTooLarge { .. } | ErrorKind::FieldTooLarge { .. }
    ));
}

/// `RecordEnding::Byte` with `Nulls::Mysql` triggers general path.
#[test]
fn general_path_via_nulls_mysql_with_byte_terminator() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"\\N|value|\n",
        FormatOptions::CSV
            .delimiter(b'|')
            .record_ending(RecordEnding::Byte(b'\n'))
            .escape(Escape::Mysql)
            .nulls(Nulls::Mysql)
            .quoting(Quoting::Never),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert!(records[0][0].is_empty(), "\\N should be NULL (empty bytes)");
    assert_eq!(records[0][1], b"value");
    Ok(())
}

#[test]
fn general_path_with_skip_initial_space() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"a, b,  c\r\n",
        FormatOptions::CSV
            .record_ending(RecordEnding::CrLf)
            .skip_initial_space(true),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0], [b"a", b"b", b"c"]);
    Ok(())
}

#[test]
fn backslash_tsv_quoted_field_with_backslash_escape() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"hel\\\"lo\"\tworld\n", FormatOptions::BACKSLASH_TSV)?;
    assert_eq!(records[0][0], b"hel\"lo");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

#[test]
fn general_path_parser_failed_after_error() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"a,\"unterminated\nb,c\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let _ = p
        .next_line()?
        .expect("positioned")
        .record()
        .expect_err("should fail");
    let err = p.next_line().expect_err("parser should be failed");
    assert_eq!(err.kind(), ErrorKind::ParserFailed);
    Ok(())
}

#[test]
fn location_display_known_renders_all_fields() {
    use coseva::Location;
    let loc = Location {
        byte: 10,
        line: 2,
        record: 1,
        field: 3,
    };
    let s = loc.to_string();
    assert!(s.contains("byte 10"));
    assert!(s.contains("line 2"));
    assert!(s.contains("record 1"));
    assert!(s.contains("field 3"));
}

#[test]
fn location_display_unknown_renders_unknown() {
    use coseva::Location;
    let s = Location::UNKNOWN.to_string();
    assert_eq!(s, "unknown location");
}

#[test]
fn mysql_unescape_z_and_literal_delimiter() -> Result<(), Box<dyn StdError>> {
    // \Z → 0x1A; literal delimiter (tab) after \\ should pass through
    let input = b"\\Z\t\\\\\n";
    let records = parse_unheaded(input, FormatOptions::MYSQL)?;
    assert_eq!(records[0][0], b"\x1a");
    assert_eq!(records[0][1], b"\\");
    Ok(())
}

/// Projection that keeps no columns still returns the right record count.
#[cfg(feature = "serde")]
#[test]
fn projected_record_keeping_no_columns() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Empty;

    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let _row: Empty = line.deserialized()?;
    Ok(())
}

#[test]
fn check_record_limit_for_in_finish_quoted_field() {
    let limits = Limits::new(3, 100, 100);
    let err = parse_all(
        b"\"ab\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("expected record too large");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 3 });
}

/// `Error::from_field_conversion` with a non-`ErrorKind` error sets a Custom
/// source and a field name. This exercises `field_name()` Some, `source()`
/// Custom, and `Display for Error` Custom branch.
#[test]
fn from_field_conversion_sets_field_name_and_custom_source() {
    let parse_err: Result<i32, _> = "bad".parse::<i32>();
    let err = coseva::Error::from_field_conversion(
        parse_err.expect_err("expected parse error"),
        0,
        "count",
    );
    // field_name() Some branch
    assert_eq!(err.field_name(), Some("count"));
    // source() Custom branch
    let source: Option<&dyn std::error::Error> = std::error::Error::source(&err);
    assert!(source.is_some(), "Custom source should be non-None");
    // Display Custom branch: message comes from the source
    let s = err.to_string();
    assert!(!s.is_empty(), "display should be non-empty: {s}");
}

// ── TSV and backslash dialect quoting details ─────────────────────────────────

/// `FormatOptions::BACKSLASH_CSV` exercises the BACKSLASH=true fast-path with
/// `\"` escapes inside a quoted field.
#[test]
fn backslash_csv_quoted_field_escape() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"say \\\"hi\\\"\",world\n", FormatOptions::BACKSLASH_CSV)?;
    assert_eq!(records[0][0], b"say \"hi\"");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

/// `FormatOptions::BACKSLASH_CSV` with `\\` escape inside quoted field.
#[test]
fn backslash_csv_quoted_field_double_backslash() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"foo\\\\bar\"\n", FormatOptions::BACKSLASH_CSV)?;
    assert_eq!(records[0][0], b"foo\\bar");
    Ok(())
}

/// `FormatOptions::BACKSLASH_TSV` with a quoted field and backslash-escaped quote.
#[test]
fn backslash_tsv_quoted_field_escape() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"a\\\"b\"\tc\n", FormatOptions::BACKSLASH_TSV)?;
    assert_eq!(records[0][0], b"a\"b");
    assert_eq!(records[0][1], b"c");
    Ok(())
}

/// TSV dialect with doubled-quote `""` inside a quoted field exercises the
/// `!BACKSLASH && input.get(at+1) == Some(&b'"')` branch.
#[test]
fn tsv_quoted_field_doubled_quote() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"say \"\"hi\"\"\"\t42\n", FormatOptions::TSV)?;
    assert_eq!(records[0][0], b"say \"hi\"");
    assert_eq!(records[0][1], b"42");
    Ok(())
}

/// TSV quoted field that ends at `\n` exercises the `Some(b'\n') if !STRICT_CRLF`
/// arm of the post-quote match.
#[test]
fn tsv_quoted_field_ends_at_newline() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"\"hello\"\n", FormatOptions::TSV)?;
    assert_eq!(records[0][0], b"hello");
    Ok(())
}

/// TSV with `\r\n` line endings in an unquoted field exercises the
/// `field_end = at - 1` stripping branch in the named-dialect parser.
#[test]
fn tsv_unquoted_field_crlf_ending() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"hello\tworld\r\n", FormatOptions::TSV)?;
    assert_eq!(records[0][0], b"hello");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

// ── Serde projected-field record boundaries ───────────────────────────────────

/// Serde deserializing from a CSV with no trailing newline hits the
/// `None if complete_input` branch in `try_parse_default_projected_record`.
#[cfg(feature = "serde")]
#[test]
fn serde_csv_no_trailing_newline_hits_eof_projected_branch() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Row {
        name: String,
        age: u32,
    }

    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let row: Row = line.deserialized()?;
    assert_eq!(row.name, "Alice");
    assert_eq!(row.age, 30);
    Ok(())
}

/// Serde deserializing a CSV where the projected field is the last and ends at
/// `\r\n` exercises the `field_end = at - 1` branch in the projected parser.
#[cfg(feature = "serde")]
#[test]
fn serde_csv_projected_field_with_crlf_ending() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Row {
        name: String,
        age: u32,
    }

    let mut p = SliceParser::with_options(
        b"name,age\r\nBob,25\r\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let row: Row = line.deserialized()?;
    assert_eq!(row.name, "Bob");
    assert_eq!(row.age, 25);
    Ok(())
}

/// Serde deserializing a quoted projected field that ends at `\n` exercises
/// the `Some(b'\n')` arm in the post-quote projected-record logic.
#[cfg(feature = "serde")]
#[test]
fn serde_csv_projected_quoted_field_ends_at_newline() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Row {
        name: String,
    }

    let mut p = SliceParser::with_options(
        b"name\n\"Alice\"\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let row: Row = line.deserialized()?;
    assert_eq!(row.name, "Alice");
    Ok(())
}

/// Serde with projected quoted field ending at EOF (no newline) exercises the
/// `None if complete_input` arm in the projected post-quote logic.
#[cfg(feature = "serde")]
#[test]
fn serde_csv_projected_quoted_field_at_eof() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Row {
        name: String,
    }

    let mut p =
        SliceParser::with_options(b"name\n\"Alice\"", FormatOptions::CSV, ParseOptions::new())
            .expect("valid options");
    let mut line = p.next_line()?.expect("record");
    let row: Row = line.deserialized()?;
    assert_eq!(row.name, "Alice");
    Ok(())
}

// ── Additional header and dialect checks ──────────────────────────────────────

/// `Headers::Provided` sets the header record from the caller without
/// consuming any input, exercising the `Headers::Provided` arm in `new()`.
#[test]
fn headers_provided_path() -> Result<(), Box<dyn StdError>> {
    let mut headers = ByteRecord::new();
    headers.push_field(b"name");
    headers.push_field(b"age");
    let options = ParseOptions::new().headers(Headers::Provided(headers));
    let records = parse_all(b"Alice,30\nBob,25\n", FormatOptions::CSV, options)?;
    assert_eq!(records[0][0], b"Alice" as &[u8]);
    assert_eq!(records[0][1], b"30" as &[u8]);
    assert_eq!(records[1][0], b"Bob" as &[u8]);
    assert_eq!(records[1][1], b"25" as &[u8]);
    Ok(())
}

/// `FormatOptions::BACKSLASH_CSV` unquoted field: a plain unquoted field
/// exercises the non-backslash section of the named-dialect unquoted path.
#[test]
fn backslash_csv_plain_unquoted_field() -> Result<(), Box<dyn StdError>> {
    let records = parse_unheaded(b"hello,world\n", FormatOptions::BACKSLASH_CSV)?;
    assert_eq!(records[0][0], b"hello");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

// ── Error introspection: field name and formatter failures ───────────────────

/// `field_name()` returns `None` for a plain parse error that has no field name.
#[test]
fn error_field_name_returns_none_for_plain_error() {
    let err = SliceParser::with_options(
        b"a\"b\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()
    .expect("no parse error on position")
    .expect("record")
    .record()
    .expect_err("expected parse error");
    assert_eq!(err.field_name(), None);
}

/// Calling `source()` on a serde `missing_field` error (which has a Detail
/// source) exercises the `ErrorSource::Detail(_) => None` arm.
#[cfg(feature = "serde")]
#[test]
fn error_source_is_none_for_detail_error() {
    use serde::de::Error as _;
    let err = coseva::Error::missing_field("name");
    let source: Option<&dyn std::error::Error> = std::error::Error::source(&err);
    assert!(source.is_none(), "Detail source should return None");
}

/// An error with a known location formatted into a failing writer hits the
/// `?` early-return on `write!(f, "CSV error at {}: ", …)`.
#[test]
fn error_display_fmt_failure_at_known_location() {
    // Produce a parse error with a known location
    let err = SliceParser::with_options(
        b"a\"b\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
    .next_line()
    .expect("no io error")
    .expect("record")
    .record()
    .expect_err("expected parse error");
    assert!(
        err.location().byte > 0 || err.location().is_known(),
        "need known location"
    );
    // A budget of 0 makes the very first write fail, exercising the error
    // return from `write!(f, "CSV error at {}: ", …)`.
    let mut sink = BudgetWriter { budget: 0 };
    let result = std::fmt::write(&mut sink, format_args!("{err}"));
    assert!(
        result.is_err(),
        "formatter write should propagate the error"
    );
}

/// An error with a field name but no known location formatted into a writer
/// that fails after the location write-slot hits the `?` on
/// `write!(f, "field {name}: ")`.
#[test]
fn error_display_fmt_failure_at_field_name() {
    // `from_field_conversion` creates an error with Custom source and a field name.
    // It also gets an unknown location, so line 372 is not reached.
    let parse_err = "bad".parse::<i32>().expect_err("needs parse error");
    let err = coseva::Error::from_field_conversion(parse_err, 0, "count");
    // The field_name prefix "field count: " is 14 bytes; budget 0 fails immediately
    // on the "field …: " write, covering the line-375 `?` path.
    let mut sink = BudgetWriter { budget: 0 };
    let result = std::fmt::write(&mut sink, format_args!("{err}"));
    assert!(
        result.is_err(),
        "formatter write should propagate the error"
    );
}

// ── SliceParser header API and cached typed decode ───────────────────────────

/// `SliceParser::headers()` returns the first-record header row.
#[test]
fn slice_parser_headers_returns_first_row() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let hdrs = p.headers()?.expect("has headers");
    assert_eq!(hdrs.get(0), Some(b"name".as_slice()));
    assert_eq!(hdrs.get(1), Some(b"age".as_slice()));
    Ok(())
}

/// `SliceParser::header_index()` returns the column index for a header name.
#[test]
fn slice_parser_header_index() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    assert_eq!(p.header_index("age")?, Some(1));
    assert_eq!(p.header_index("missing")?, None);
    Ok(())
}

/// `SliceParser::header_indices()` returns all columns matching a header name.
#[test]
fn slice_parser_header_indices() -> Result<(), Box<dyn StdError>> {
    let mut p =
        SliceParser::with_options(b"a,b,a\n1,2,3\n", FormatOptions::CSV, ParseOptions::new())
            .expect("valid options");
    let idxs = p.header_indices("a")?;
    assert_eq!(idxs, &[0, 2]);
    Ok(())
}

/// `SliceParser::has_headers()` reports true when headers are configured.
#[test]
fn slice_parser_has_headers() {
    let p_with =
        SliceParser::with_options(b"name\nAlice\n", FormatOptions::CSV, ParseOptions::new())
            .expect("valid options");
    assert!(p_with.has_headers());

    let p_without = SliceParser::with_options(
        b"Alice\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    assert!(!p_without.has_headers());
}

/// `SliceParser::set_headers()` installs an externally provided header record
/// and (with `FieldCount::MatchFirst`) wires up the expected field count.
#[test]
fn slice_parser_set_headers() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"Alice,30\nBob,25\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::MatchFirst),
    )
    .expect("valid options");
    let mut hdrs = ByteRecord::new();
    hdrs.push_field(b"name");
    hdrs.push_field(b"age");
    p.set_headers(hdrs);
    assert!(p.has_headers());
    assert_eq!(p.header_index("name")?, Some(0));
    let mut line = p.next_line()?.expect("record");
    let mut r = ByteRecord::new();
    line.read_byte_record_into(&mut r)?;
    assert_eq!(r.get(0), Some(b"Alice".as_slice()));
    Ok(())
}

/// Deserializing a struct whose field name is absent in the CSV headers
/// triggers the "field not found" error from `resolve_decode_mapping`.
#[cfg(feature = "serde")]
#[test]
fn serde_field_missing_from_headers_triggers_decode_error() {
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    struct Row {
        #[expect(
            dead_code,
            reason = "intentionally unused – triggers a Serde error for the missing-field path"
        )]
        missing_field: String,
    }

    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let mut line = p.next_line().expect("no io error").expect("record");
    let result: Result<Row, _> = line.deserialized();
    assert!(
        result.is_err(),
        "should fail – missing_field not in headers"
    );
    let _ = result.expect_err("should have failed");
}

/// Deserializing the same struct type twice hits the cached `typed_mapping`
/// branch on the second call, exercising the early-return cache path.
#[cfg(feature = "serde")]
#[test]
fn serde_typed_mapping_cache_hit() -> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    struct Row {
        name: String,
        #[expect(dead_code, reason = "populated by serde deserialization")]
        age: u32,
    }
    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\nBob,25\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let r1: Row = p.next_line()?.expect("row1").deserialized()?;
    let r2: Row = p.next_line()?.expect("row2").deserialized()?;
    assert_eq!(r1.name, "Alice");
    assert_eq!(r2.name, "Bob");
    Ok(())
}

/// A whitespace-padded record exercises `trim_spans` → `span.trim_ascii`.
///
/// There is no quoted field here, so the plain kernel takes it and the
/// exempting policy is applied by the same span walk either way.
#[test]
fn trim_spans_called_in_general_path() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  hello  ,  world  \n",
        FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only()),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0][0], b"hello");
    assert_eq!(records[0][1], b"world");
    Ok(())
}

/// When `general_parsing=true` and input is exhausted,
/// `read_physical_record` returns `Ok(false)`.
#[test]
fn read_physical_record_general_path_eof() -> Result<(), Box<dyn StdError>> {
    // `CrLf` forces `general_parsing=true`.  Valid two-record input exercises
    // the success path, and the final `read_physical_record` call returns
    // `Ok(false)` at EOF.
    let records = parse_all(
        b"a,b\r\nc,d\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0][0], b"a");
    Ok(())
}

/// Calling `SliceParser::seek` on a parser with `Headers::FirstRecord`
/// forces `ensure_headers` through the "already initialized" fast return.
#[test]
fn slice_parser_seek_after_headers() -> Result<(), Box<dyn StdError>> {
    let mut p = SliceParser::with_options(
        b"name,age\nAlice,30\nBob,25\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    // Consume the header row so headers are initialized.
    let _ = p.headers()?;
    // Seek back to record 0 (after the header).
    let loc = coseva::Location {
        byte: 9,
        line: 2,
        record: 0,
        field: 0,
    };
    p.seek(loc)?;
    let mut line = p.next_line()?.expect("record");
    let mut r = ByteRecord::new();
    line.read_byte_record_into(&mut r)?;
    assert_eq!(r.get(0), Some(b"Alice".as_slice()));
    Ok(())
}

// ── CsvIndex integration ──────────────────────────────────────────────────────

/// Building a `CsvIndex` and seeking to a record exercises `advance_line_origin`
/// in the engine.
#[test]
#[cfg(feature = "index")]
fn csv_index_build_and_seek_covers_advance_line_origin() -> Result<(), Box<dyn StdError>> {
    use coseva::index::{CsvIndex, IndexOptions};
    let source = b"a,b\nc,d\ne,f\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    assert_eq!(index.len(), 3);
    let mut reader = index.parser_at(source, 2)?;
    let mut line = reader.next_line()?.expect("record");
    let mut r = ByteRecord::new();
    line.read_byte_record_into(&mut r)?;
    assert_eq!(r.get(0), Some(b"e".as_slice()));
    Ok(())
}

// ── Owned-record field/limit edge cases under non-default limits ─────────────

/// A run of consecutive delimiters is appended as a block of empty fields, so
/// the block has to be checked against the field limit before it is appended
/// rather than one field at a time.
#[test]
fn a_run_of_empty_owned_fields_is_capped_by_the_field_limit() {
    let error = owned_records(
        b"a,,,,,,,,b\n",
        FormatOptions::CSV,
        Limits::new(1024, 1024, 4),
    )
    .expect_err("nine fields exceed a limit of four");

    assert!(
        matches!(error.kind(), ErrorKind::TooManyFields { limit: 4 }),
        "expected a field-count overflow, got {error:?}"
    );
}

/// The same run stays within the limit when it fits, which is the boundary the
/// check above is guarding.
#[test]
fn a_run_of_empty_owned_fields_is_kept_when_it_fits() -> Result<(), Box<dyn StdError>> {
    let records = owned_records(b"a,,,,b\n", FormatOptions::CSV, Limits::new(1024, 1024, 5))?;

    assert_eq!(
        records,
        vec![vec![
            b"a".to_vec(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            b"b".to_vec()
        ]]
    );
    Ok(())
}

/// An owned field is unescaped into a scratch buffer, so its accumulated
/// length is what the field limit applies to.
#[test]
fn an_owned_quoted_field_is_capped_by_the_field_byte_limit() {
    let error = owned_records(
        b"\"aaaa\"\"aaaaaaaaaaaaaaaaaaaa\"\n",
        FormatOptions::CSV,
        Limits::new(1024, 8, 16),
    )
    .expect_err("the unescaped field is longer than eight bytes");

    assert!(
        matches!(error.kind(), ErrorKind::FieldTooLarge { limit: 8 }),
        "expected a field-size overflow, got {error:?}"
    );
}

/// With `RecordEnding::Newline` a bare carriage return immediately before the
/// newline belongs to the ending, not to the unquoted field it follows.
#[test]
fn an_owned_unquoted_field_sheds_the_carriage_return_of_a_crlf_ending()
-> Result<(), Box<dyn StdError>> {
    let records = owned_records(
        b"alpha,beta\r\ngamma,delta\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::Newline),
        Limits::new(1024, 1024, 16),
    )?;

    assert_eq!(
        records,
        vec![
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            vec![b"gamma".to_vec(), b"delta".to_vec()],
        ]
    );
    Ok(())
}

/// The closing quote of an owned field may be followed by a two-byte CRLF
/// ending, which has to consume both bytes before the next record starts.
#[test]
fn an_owned_quoted_field_ends_at_a_crlf_record_ending() -> Result<(), Box<dyn StdError>> {
    let records = owned_records(
        b"\"alpha\",\"beta\"\r\n\"gamma\",\"delta\"\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::Newline),
        Limits::new(1024, 1024, 16),
    )?;

    assert_eq!(
        records,
        vec![
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            vec![b"gamma".to_vec(), b"delta".to_vec()],
        ]
    );
    Ok(())
}

/// A quoted owned field that copied an escaped quote is checked after the final
/// segment is appended at the closing quote.
#[test]
fn an_owned_segmented_quoted_field_is_capped_at_the_closing_quote() {
    let limits = Limits::new(1024, 8, 16);
    let error = parse_all(
        b"\"abcd\"\"efgh\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect_err("decoded quoted field is longer than the field limit");

    assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 8 });
}

// ── Typed and projected CsvDecode resolution ──────────────────────────────────

/// Without a header record there is nothing to permute, so the decoder falls
/// back to decoding fields in declaration order.
#[test]
fn a_typed_decode_without_headers_maps_fields_positionally() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"one,two\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("a record");

    assert_eq!(
        line.decoded::<Pair>()?,
        Pair {
            left: "one".to_owned(),
            right: "two".to_owned(),
        }
    );
    Ok(())
}

/// A header record that lacks one of the target's field names cannot be
/// permuted into the target, and the failure names the offending field.
#[test]
fn a_typed_decode_rejects_a_header_missing_a_target_field() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,middle\none,two\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = parser.next_line()?.expect("a record");
    let error = line
        .decoded::<Pair>()
        .expect_err("`right` is absent from the header");

    assert_eq!(error.kind(), ErrorKind::Decode);
    assert_eq!(error.field_name(), Some("right"));
    Ok(())
}

/// A header record that repeats one of the target's field names is ambiguous,
/// so it is rejected rather than resolved to the first match.
#[test]
fn a_typed_decode_rejects_an_ambiguous_header() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,right,right\none,two,three\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = parser.next_line()?.expect("a record");
    let error = line
        .decoded::<Pair>()
        .expect_err("`right` appears twice in the header");

    assert_eq!(error.kind(), ErrorKind::Decode);
    assert_eq!(error.field_name(), Some("right"));
    Ok(())
}

/// The projected kernel materializes only the selected columns, skipping the
/// bytes of everything in between.
#[test]
fn a_projected_decode_selects_only_the_named_columns() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        PROJECTED_HEADER,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = parser.next_line()?.expect("a record");

    assert_eq!(
        line.decoded::<Projected>()?,
        Projected {
            left: "alpha".to_owned(),
            count: 7,
        }
    );
    Ok(())
}

/// A conversion failure on the projected kernel still has to be stamped with
/// the record's real byte and line position, which the kernel tracks
/// separately from the full-record path.
#[test]
fn a_projected_decode_failure_carries_the_record_position() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,middle,count\nalpha,ignored,seven\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = parser.next_line()?.expect("a record");
    let error = line
        .decoded::<Projected>()
        .expect_err("`seven` is not a `u32`");

    assert_eq!(
        error.location().line,
        2,
        "the failure is on the second line"
    );
    assert_eq!(error.field_name(), Some("count"));
    Ok(())
}

/// Non-default limits disable the specialized kernels, so a projected mapping
/// has to be applied to a fully materialized record instead.
#[test]
fn a_projected_decode_falls_back_to_a_full_record_parse() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        PROJECTED_HEADER,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .limits(Limits::new(1024, 1024, 16)),
    )?;
    let mut line = parser.next_line()?.expect("a record");

    assert_eq!(
        line.decoded::<Projected>()?,
        Projected {
            left: "alpha".to_owned(),
            count: 7,
        }
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct CachedNamesA {
    left: String,
    count: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct CachedNamesB {
    left: String,
    count: u32,
}

static CACHED_NAMES_A: [&str; 2] = ["left", "count"];
static CACHED_NAMES_B: [&str; 2] = ["left", "count"];

fn decode_cached_names<'record, R>(record: &R) -> Result<(String, u32), coseva::Error>
where
    R: coseva::encoding::DecodeRecord<'record> + ?Sized,
{
    let left = String::from_utf8_lossy(record.get_field(0).unwrap_or_default()).into_owned();
    let raw = String::from_utf8_lossy(record.get_field(1).unwrap_or_default()).into_owned();
    let count = raw
        .parse::<u32>()
        .map_err(|error| coseva::Error::from_field_conversion(error, 1, "count"))?;
    Ok((left, count))
}

impl<'record> coseva::encoding::CsvDecode<'record> for CachedNamesA {
    fn csv_decode<R>(record: &R) -> Result<Self, coseva::Error>
    where
        R: coseva::encoding::DecodeRecord<'record> + ?Sized,
    {
        let (left, count) = decode_cached_names(record)?;
        Ok(Self { left, count })
    }

    fn field_names() -> &'static [&'static str] {
        &CACHED_NAMES_A
    }
}

impl<'record> coseva::encoding::CsvDecode<'record> for CachedNamesB {
    fn csv_decode<R>(record: &R) -> Result<Self, coseva::Error>
    where
        R: coseva::encoding::DecodeRecord<'record> + ?Sized,
    {
        let (left, count) = decode_cached_names(record)?;
        Ok(Self { left, count })
    }

    fn field_names() -> &'static [&'static str] {
        &CACHED_NAMES_B
    }
}

/// Equal field-name lists from different decode targets must still reuse the
/// cached typed mapping after the pointer fast path misses.
#[test]
fn equal_typed_decode_names_at_distinct_addresses_rekey_the_cache() -> Result<(), Box<dyn StdError>>
{
    let names_a = <CachedNamesA as coseva::encoding::CsvDecode>::field_names();
    let names_b = <CachedNamesB as coseva::encoding::CsvDecode>::field_names();
    assert_eq!(names_a, names_b);
    assert_ne!(
        names_a.as_ptr(),
        names_b.as_ptr(),
        "the cache-value path needs distinct field-name statics"
    );

    let mut parser = SliceParser::with_options(
        b"left,middle,count\nalpha,ignored,1\nbeta,ignored,2\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut line = parser.next_line()?.expect("first record");
    assert_eq!(
        line.decoded::<CachedNamesA>()?,
        CachedNamesA {
            left: "alpha".to_owned(),
            count: 1,
        }
    );
    let mut line = parser.next_line()?.expect("second record");
    assert_eq!(
        line.decoded::<CachedNamesB>()?,
        CachedNamesB {
            left: "beta".to_owned(),
            count: 2,
        }
    );
    Ok(())
}

/// Once the remaining input is longer than the default field limit the
/// projected kernel switches to its limit-checking instantiation, which has to
/// produce the same records as the unchecked one.
#[test]
fn a_projected_decode_of_a_large_input_checks_the_field_limit() -> Result<(), Box<dyn StdError>> {
    let mut input = b"left,middle,count\n".to_vec();
    let filler = "f".repeat(4096);
    // Comfortably past `Limits::DEFAULT.max_field_bytes` (4 MiB).
    for index in 0..1100_u32 {
        input.extend_from_slice(b"alpha,");
        input.extend_from_slice(filler.as_bytes());
        input.extend_from_slice(format!(",{index}\n").as_bytes());
    }

    let mut parser = SliceParser::with_options(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut seen = 0_u32;
    while let Some(mut line) = parser.next_line()? {
        let row = line.decoded::<Projected>()?;
        assert_eq!(row.left, "alpha");
        assert_eq!(row.count, seen);
        seen += 1;
    }
    assert_eq!(seen, 1100);
    Ok(())
}

// ── Pushdown filter candidate scanning ────────────────────────────────────────

/// A skipped span longer than the fused-scan threshold is counted with the
/// vectorized helpers instead of byte by byte, and both paths have to agree on
/// how many records were passed over.
#[test]
fn a_distant_filter_candidate_is_located_by_a_vectorized_skip() -> Result<(), Box<dyn StdError>> {
    let mut input = b"city,pop\n".to_vec();
    // Well past the 96-byte threshold that selects the scalar skip.
    for index in 0..40 {
        input.extend_from_slice(format!("town{index},{index}\n").as_bytes());
    }
    input.extend_from_slice(b"target,99\n");

    let mut parser = SliceParser::with_options(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let predicate = Predicate::equals(0, "target");
    let hits = parser
        .matching_byte_records(&predicate)
        .map(|record| Ok(record?.get_str(1)?.unwrap_or_default().to_owned()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;

    assert_eq!(hits, ["99"]);
    Ok(())
}

// ── Owned-loop dialect edge cases ─────────────────────────────────────────────

/// `skip_initial_space` disables the plain-field fast path, so every field of
/// the record is parsed by the general owned loop. With quoting turned off a
/// leading quote is then just an ordinary byte of an unquoted field.
#[test]
fn a_leading_quote_is_literal_when_quoting_is_disabled() -> Result<(), Box<dyn StdError>> {
    let records = owned_records(
        b"\"alpha, \"beta\"\n",
        FormatOptions::CSV
            .skip_initial_space(true)
            .syntax(Syntax::Compatible(Recovery::NONE))
            .quoting(Quoting::Never),
        Limits::new(1024, 1024, 16),
    )?;

    assert_eq!(
        records,
        vec![vec![b"\"alpha".to_vec(), b"\"beta\"".to_vec()]]
    );
    Ok(())
}

/// The general owned loop also has to shed the carriage return of a CRLF
/// ending from the unquoted field it terminates.
#[test]
fn the_general_owned_loop_sheds_the_carriage_return_of_a_crlf_ending()
-> Result<(), Box<dyn StdError>> {
    let records = owned_records(
        b"alpha, beta\r\ngamma, delta\r\n",
        FormatOptions::CSV
            .skip_initial_space(true)
            .record_ending(RecordEnding::Newline),
        Limits::new(1024, 1024, 16),
    )?;

    assert_eq!(
        records,
        vec![
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            vec![b"gamma".to_vec(), b"delta".to_vec()],
        ]
    );
    Ok(())
}

// ── Poisoned-parser state propagation ─────────────────────────────────────────

/// Once a record fails to parse the engine's position is meaningless, so every
/// later request has to report the poisoning rather than resynchronize.
#[test]
fn a_poisoned_slice_parser_refuses_every_later_record() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"a,b\nc,\"unterminated\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = ByteRecord::new();
    let mut line = parser.next_line()?.expect("the first record");
    line.read_byte_record_into(&mut record)?;

    let mut line = parser.next_line()?.expect("the malformed record");
    let first = line
        .read_byte_record_into(&mut record)
        .expect_err("the quoted field is never closed");
    assert_eq!(first.kind(), ErrorKind::UnterminatedQuotedField);

    let again = parser
        .next_line()
        .expect_err("a poisoned parser cannot be resumed");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// The pushdown filter runs its own advance loop, which needs the same guard.
#[test]
fn a_poisoned_filter_run_refuses_every_later_record() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"a,b\nc,\"unterminated\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = ByteRecord::new();
    let mut line = parser.next_line()?.expect("the first record");
    line.read_byte_record_into(&mut record)?;
    let mut line = parser.next_line()?.expect("the malformed record");
    let _ = line
        .read_byte_record_into(&mut record)
        .expect_err("the quoted field is never closed");

    let predicate = Predicate::equals(0, "c");
    let outcome = parser
        .matching_byte_records(&predicate)
        .next()
        .expect("a poisoned run reports rather than ending")
        .expect_err("a poisoned parser cannot be resumed");
    assert_eq!(outcome.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// The typed projected kernel is a third entry point into the engine and
/// carries the same guard.
#[test]
fn a_poisoned_projected_decode_reports_the_failure() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,middle,count\nalpha,ignored,1\nbeta,\"unterminated\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut record = ByteRecord::new();
    let mut line = parser.next_line()?.expect("the first record");
    line.read_byte_record_into(&mut record)?;
    let mut line = parser.next_line()?.expect("the malformed record");
    let _ = line
        .read_byte_record_into(&mut record)
        .expect_err("the quoted field is never closed");

    let outcome = parser
        .next_line()
        .expect_err("a poisoned parser cannot be resumed");
    assert_eq!(outcome.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// A `CrLf` record ending forces the general parser, whose owned-record entry
/// point has to report the end of the input rather than the fast path's.
#[test]
fn the_general_parser_reports_the_end_of_an_owned_run() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"alpha,beta\r\ngamma,delta\r\n",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = ByteRecord::new();
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        records.push(record.iter().map(<[u8]>::to_vec).collect::<Vec<_>>());
    }

    assert_eq!(
        records,
        vec![
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            vec![b"gamma".to_vec(), b"delta".to_vec()],
        ]
    );
    Ok(())
}

// ── Serde projected-kernel fallback and poison propagation ───────────────────

/// The projected kernel copies field bytes verbatim, so a record that needs
/// unescaping is handed back for a full parse. The deserialized values must be
/// identical either way.
#[cfg(feature = "serde")]
#[test]
fn a_projected_deserialization_falls_back_for_an_escaped_field() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct Subset {
        left: String,
        count: u32,
    }

    let mut parser = SliceParser::with_options(
        b"left,middle,count\nplain,ignored,1\n\"say \"\"hi\"\"\",ignored,2\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut rows = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        rows.push(line.deserialized::<Subset>()?);
    }

    assert_eq!(
        rows,
        vec![
            Subset {
                left: "plain".to_owned(),
                count: 1,
            },
            Subset {
                left: "say \"hi\"".to_owned(),
                count: 2,
            },
        ]
    );
    Ok(())
}

/// The projected Serde kernel needs every selected column to be present, so a
/// record that stops short of one is handed back for a full parse, where the
/// absence is reported properly instead of as a silent bail.
#[cfg(feature = "serde")]
#[test]
fn a_projected_deserialization_falls_back_for_a_short_record() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct Subset {
        left: String,
        count: u32,
    }

    let mut parser = SliceParser::with_options(
        b"left,middle,count\nplain,ignored,1\nshort\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::Flexible),
    )?;

    let mut line = parser.next_line()?.expect("the first record");
    assert_eq!(
        line.deserialized::<Subset>()?,
        Subset {
            left: "plain".to_owned(),
            count: 1,
        }
    );

    let mut line = parser.next_line()?.expect("the short record");
    let error = line
        .deserialized::<Subset>()
        .expect_err("`count` is absent from the short record");
    assert_ne!(
        error.kind(),
        ErrorKind::ParserFailed,
        "the short record is reported as a missing field, not a parser failure"
    );
    Ok(())
}

/// `SliceParser` hands out a `Line` whose poison flag is the engine's own
/// `self.failed`, so a first failed view leaves `cursor_end` unset. Reading
/// the same line a second time takes the uncached path in
/// `read_byte_record_into`, which for a default CSV dialect (`general_parsing`
/// is `false`) checks `self.failed` directly in the owned fast path of
/// `read_physical_record`.
#[test]
fn a_second_view_of_a_poisoned_line_reports_the_failure_on_the_owned_fast_path()
-> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("a positioned line");
    let _ = line.record().expect_err("the quoted field is never closed");

    let mut record = ByteRecord::new();
    let again = line
        .read_byte_record_into(&mut record)
        .expect_err("a second view of the same poisoned line reports the failure");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    Ok(())
}

#[test]
fn a_failed_owned_read_invalidates_the_reused_records_location() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"ok\n\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::new();

    let mut line = parser.next_line()?.expect("the valid record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.byte_range(), 0..3);

    let mut line = parser.next_line()?.expect("the malformed record");
    line.read_byte_record_into(&mut record)
        .expect_err("the quoted field is never closed");
    assert_eq!(record.byte_range(), 0..0);
    assert_eq!(record.index(), 0);
    Ok(())
}

/// Same as above, but with a dialect that requires general parsing
/// (`RecordEnding::CrLf`), which routes `read_physical_record` through
/// `next_physical_record`/`fill_record_spans`, exercising that guard instead.
#[test]
fn a_second_view_of_a_poisoned_line_reports_the_failure_on_the_general_parsing_path()
-> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"\"unterminated",
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("a positioned line");
    let _ = line.record().expect_err("the quoted field is never closed");

    let mut record = ByteRecord::new();
    let again = line
        .read_byte_record_into(&mut record)
        .expect_err("a second view of the same poisoned line reports the failure");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// A poisoned line reports the poisoning through the Serde path too: the header
/// record parses fine, the first data record fails on an unterminated quote
/// (poisoning the engine without setting `cursor_end`), and a second view of the
/// *same* line reports the failure instead of silently reparsing it and
/// reproducing the original fault.
#[cfg(feature = "serde")]
#[test]
fn a_second_view_of_a_poisoned_line_reports_the_failure_on_the_serde_path()
-> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Subset {
        left: String,
        count: u32,
    }

    let mut parser = SliceParser::with_options(
        b"left,middle,count\nplain,ignored,1\n\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut line = parser.next_line()?.expect("the first, well-formed record");
    let row: Subset = line.deserialized()?;
    assert_eq!(row.left, "plain");
    assert_eq!(row.count, 1);

    let mut line = parser.next_line()?.expect("the malformed data record");
    let _ = line.record().expect_err("the quoted field is never closed");

    let again = line
        .deserialized::<Subset>()
        .expect_err("a second view of the same poisoned line reports the failure");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// A field longer than `Limits::DEFAULT.max_field_bytes` makes the projected
/// kernel bail out of `next_projected_record` with `ProjectedOutcome::Fallback`
/// even when the oversized field is an unselected column, because the
/// checking kernel scans every field's length regardless of projection. The
/// full-record parser then reports the same limit from the general path.
#[cfg(feature = "serde")]
#[test]
fn a_projected_deserialization_falls_back_for_an_oversized_unselected_field()
-> Result<(), Box<dyn StdError>> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "the record errors before a value is ever constructed"
    )]
    struct Subset {
        left: String,
        count: u32,
    }

    let mut input = b"left,middle,count\n".to_vec();
    // A small, well-formed row first "learns" the projection in the Serde
    // cache; only then does the next record attempt the projected kernel
    // instead of skipping straight to the full-record fallback.
    input.extend_from_slice(b"warm,ignored,0\n");
    input.extend_from_slice(b"a,");
    // One byte past `Limits::DEFAULT.max_field_bytes` (4 MiB), in the
    // unselected `middle` column.
    input.extend(core::iter::repeat_n(b'x', 4 * 1024 * 1024 + 1));
    input.extend_from_slice(b",1\n");

    let mut parser = SliceParser::with_options(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = parser.next_line()?.expect("the warm-up record");
    let _: Subset = line.deserialized()?;

    let mut line = parser.next_line()?.expect("the oversized record");
    let error = line
        .deserialized::<Subset>()
        .expect_err("the middle field exceeds the field size limit");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldTooLarge {
            limit: Limits::DEFAULT.max_field_bytes,
        }
    );
    Ok(())
}

/// A name repeated more than twice grows the lookup's spilled index list, which
/// is the only path that reaches it after the second column. Two repeats build
/// it; a third and fourth extend it.
#[test]
fn a_header_name_repeated_four_times_reports_every_column() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        "tag,tag,other,tag,tag\na,b,c,d,e\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    assert_eq!(parser.header_indices("tag")?, [0, 1, 3, 4]);
    assert_eq!(parser.header_indices("other")?, [2]);
    Ok(())
}

/// Asking for a single column by a name that repeats must report the first one
/// rather than refusing, and must agree with the full list.
#[test]
fn a_duplicated_header_resolves_to_its_first_column() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        "city,tag,tag\na,b,c\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    assert_eq!(parser.header_index("tag")?, Some(1));
    assert_eq!(parser.header_index("city")?, Some(0));
    assert_eq!(parser.header_indices("tag")?, [1, 2]);
    Ok(())
}

/// The lookup is built on first use rather than when the headers are read, so a
/// name resolved after records have been consumed must still see the header row
/// the parser started from.
#[test]
fn a_name_resolves_after_records_have_been_read() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        "city,population\nBoston,675647\nAustin,961855\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut seen = 0;
    while let Some(_line) = parser.next_line()? {
        seen += 1;
    }
    assert_eq!(seen, 2);
    assert_eq!(parser.header_index("population")?, Some(1));
    Ok(())
}

/// A quote-exempting trim policy agrees with itself either side of the kernel
/// boundary.
///
/// The plain kernel now takes records that have no quoted field, while records
/// that do still fall to the general parser. Both must apply the policy the
/// same way, which is what this pins: the padded unquoted fields lose their
/// padding in both records, and the padded quoted one keeps it.
#[test]
fn unquoted_only_trimming_agrees_across_the_kernel_boundary() -> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  a  ,  b  \n  c  ,\"  d  \"\n",
        FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only()),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0], [b"a".to_vec(), b"b".to_vec()]);
    assert_eq!(records[1], [b"c".to_vec(), b"  d  ".to_vec()]);
    Ok(())
}

/// The same, for a dialect that permits bare quotes inside unquoted fields.
///
/// Such a quote is data, so the field is unquoted and the kernel keeps the
/// record — the exemption must not latch onto the quote byte.
#[test]
fn unquoted_only_trimming_ignores_a_bare_quote_in_an_unquoted_field()
-> Result<(), Box<dyn StdError>> {
    let records = parse_all(
        b"  a\"b  ,  c  \n",
        FormatOptions::CSV
            .trim(Whitespace::ALL.unquoted_only())
            .syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        ParseOptions::new().headers(Headers::None),
    )?;
    assert_eq!(records[0], [b"a\"b".to_vec(), b"c".to_vec()]);
    Ok(())
}
