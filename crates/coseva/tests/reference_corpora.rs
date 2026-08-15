//! Licensed external CSV reference corpora (TODO T9).
//!
//! Adopts a compact, individually attributed set of CSV parsing/rejection
//! cases from two independently maintained, permissively licensed upstream
//! test suites, and runs every one of them through all three coseva front
//! ends -- [`SliceParser`], a short-reading [`IoParser`], and a one-byte-fed
//! [`PushParser`] -- plus the matching [`coseva::format`] static type where a
//! built-in exists for the case's format, with an emitter round trip for
//! every case that parses successfully.
//!
//! Full provenance (upstream URL, pinned revision/SHA, license/SPDX
//! identifier, per-case adaptation notes, and the format profile used) lives
//! in `fixtures/reference/manifest.json`; the license texts covering the
//! adopted material are in `fixtures/reference/licenses/`, and
//! `fixtures/reference/README.md` explains the corpus structure, the sources
//! considered and excluded, and the intentional semantic differences a few
//! cases assert. This file contains no vendored source code, only CSV byte
//! literals and their expected parse results, both individually attributed
//! by doc comment and cross-referenced to the manifest by case id.
//!
//! # Sources
//!
//! - **`go/*`** -- 34 cases from the Go standard library's `encoding/csv`
//!   `readTests` table (BSD-3-Clause), pinned at commit
//!   `9549c91031e12f484d5b02d4531f9022231f951f`. Go's table annotates its
//!   `Input` strings with position-marker glyphs (`§`, `¶`, `∑`) that its own
//!   driver strips before parsing; those glyphs are removed here too and are
//!   not CSV data.
//! - **`rust_csv/*`** -- 11 cases from BurntSushi/rust-csv's `src/reader.rs`
//!   unit tests (Unlicense), pinned at commit
//!   `05612e87e6e92910d40d7214b70952b8434fef9b`.
//!
//! # Semantic differences, not failures
//!
//! A few cases intentionally assert a *different* result than the upstream
//! implementation produces for the same bytes, because coseva makes a
//! different, documented design choice (embedded CRLF in a quoted field is
//! not normalized, a bare trailing CR is retained as data rather than
//! stripped, a field-count mismatch poisons the rest of the parse rather than
//! being skipped, and `headers()` on empty input is `Ok(None)` rather than an
//! empty-but-present record). Each such test's doc comment says so, and
//! `fixtures/reference/README.md` collects the full list.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(feature = "std")]
#![expect(
    clippy::expect_used,
    reason = "a reference corpus that doesn't parse or assert as documented must fail loudly, not be silently skipped"
)]

use std::io::Read;

use coseva::config::{
    BlankRecords, EmitOptions, FieldCount, FormatOptions, Headers, ParseOptions, Whitespace,
};
use coseva::format::{CommentedCsv, Csv, CsvFormat, Semicolon, StaticFormat, TrimmedCsv};
use coseva::{ByteRecord, ErrorKind, IoParser, PushParser, SliceParser, TextRecord, VecEmitter};

// ── Shared result shape ──────────────────────────────────────────────────────

/// What a front end produced on success: the header row, if any, and every
/// data row, all as raw bytes so invalid UTF-8 round-trips exactly like any
/// other byte.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    headers: Option<Vec<Vec<u8>>>,
    rows: Vec<Vec<Vec<u8>>>,
}

/// The single terminal outcome a front end reaches for a whole document: a
/// full parse, or the category of the first error encountered. [`Location`]
/// is deliberately dropped, since these cases compare parsed content and
/// rejection category, not exact byte/line/field positions.
///
/// [`Location`]: coseva::Location
type FrontResult = Result<Parsed, ErrorKind>;

fn header_row(fields: &[&[u8]]) -> Vec<Vec<u8>> {
    fields.iter().map(|f| f.to_vec()).collect()
}

fn rows(data: &[&[&[u8]]]) -> Vec<Vec<Vec<u8>>> {
    data.iter()
        .map(|record| record.iter().map(|field| field.to_vec()).collect())
        .collect()
}

/// Build the expected success value for a case.
#[expect(
    clippy::unnecessary_wraps,
    reason = "callers assign this to a `FrontResult`-typed `expected` binding alongside \
              sibling `Err(ErrorKind::...)` expressions for rejection cases"
)]
fn ok_case(headers: Option<&[&[u8]]>, data: &[&[&[u8]]]) -> FrontResult {
    Ok(Parsed {
        headers: headers.map(header_row),
        rows: rows(data),
    })
}

// ── Format profiles (see fixtures/reference/manifest.json's format_profiles) ─

const FMT_DEFAULT: FormatOptions = FormatOptions::CSV;
const FMT_SEMICOLON: FormatOptions = FormatOptions::SEMICOLON;
const FMT_COMMENTED: FormatOptions = FormatOptions::COMMENTED_CSV;
const FMT_BLANK_SKIP: FormatOptions = FormatOptions::CSV.blank_records(BlankRecords::Skip);
const FMT_TRIM_ALL: FormatOptions = FormatOptions::TRIMMED_CSV;
const FMT_TRIM_HEADERS: FormatOptions = FormatOptions::CSV.trim(Whitespace::HEADERS);
const FMT_TRIM_FIELDS: FormatOptions = FormatOptions::CSV.trim(Whitespace::FIELDS);
/// Invalid at construction: a delimiter cannot also be a record terminator.
const FMT_DELIMITER_IS_NEWLINE: FormatOptions = FormatOptions::CSV.delimiter(b'\n');
/// Invalid at construction: the comment byte collides with the delimiter.
const FMT_COMMENT_EQ_DELIMITER: FormatOptions = FormatOptions::CSV.comment(Some(b','));

fn opts_none() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn opts_headers() -> ParseOptions {
    ParseOptions::new()
}

fn opts_none_match_first() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .field_count(FieldCount::MatchFirst)
}

// ── A short-reading `Read` for the `IoParser` front end ──────────────────────

/// A reader that yields at most two bytes per call, so `IoParser` refills its
/// window mid-record on every case here, rather than only at whatever seam a
/// single large read happens to land on.
struct ShortReader {
    data: Vec<u8>,
    pos: usize,
}

impl ShortReader {
    fn new(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            pos: 0,
        }
    }
}

impl Read for ShortReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.data.len() - self.pos;
        let take = available.min(buf.len()).min(2);
        buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

// ── Generic collection, one front end each, shared across dynamic/static ────

/// Drain every line from a [`SliceParser`], materializing headers (when
/// requested) and every data row as owned bytes.
fn drain_slice<F: CsvFormat>(
    parser: &mut SliceParser<'_, F>,
    want_headers: bool,
) -> Result<Parsed, coseva::Error> {
    let mut rows = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    let headers = if want_headers {
        parser
            .headers()?
            .map(|h| h.iter().map(<[u8]>::to_vec).collect())
    } else {
        None
    };
    Ok(Parsed { headers, rows })
}

/// Drain every line from an [`IoParser`]; see [`drain_slice`].
fn drain_io<R: Read, F: CsvFormat>(
    parser: &mut IoParser<R, F>,
    want_headers: bool,
) -> Result<Parsed, coseva::Error> {
    let mut rows = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    let headers = if want_headers {
        parser
            .headers()?
            .map(|h| h.iter().map(<[u8]>::to_vec).collect())
    } else {
        None
    };
    Ok(Parsed { headers, rows })
}

/// Feed a [`PushParser`] one byte at a time and drain every line it
/// completes; see [`drain_slice`].
fn drain_push<F: CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    want_headers: bool,
) -> Result<Parsed, coseva::Error> {
    let mut rows = Vec::new();
    for byte in input.chunks(1) {
        let mut fed = 0;
        while fed < byte.len() {
            fed += drain_push_chunk(parser, &byte[fed..], &mut rows)?;
        }
    }
    parser.finish();
    let _ = drain_push_chunk(parser, b"", &mut rows)?;
    let headers = if want_headers {
        parser
            .headers()
            .map(|h| h.iter().map(<[u8]>::to_vec).collect())
    } else {
        None
    };
    Ok(Parsed { headers, rows })
}

/// Lend one chunk to a [`PushParser`], collect the lines it completes, and
/// report how many bytes it absorbed.
fn drain_push_chunk<F: CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    rows: &mut Vec<Vec<Vec<u8>>>,
) -> Result<usize, coseva::Error> {
    let mut chunk = parser.chunk(input);
    loop {
        match chunk.next_line() {
            Ok(Some(mut line)) => {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)?;
                rows.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            Ok(None) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(chunk.done())
}

// ── Front-end wrappers: construct, drain, reduce to `FrontResult` ───────────

fn run_slice_dynamic(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = SliceParser::with_options(input, format, options)?;
        drain_slice(&mut parser, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

fn run_slice_static<F: StaticFormat>(
    input: &[u8],
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = SliceParser::<F>::new(input, options)?;
        drain_slice(&mut parser, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

fn run_io_dynamic(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = IoParser::with_options(ShortReader::new(input), format, options)?;
        drain_io(&mut parser, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

fn run_io_static<F: StaticFormat>(
    input: &[u8],
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = IoParser::<_, F>::new(ShortReader::new(input), options)?;
        drain_io(&mut parser, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

fn run_push_dynamic(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = PushParser::with_options(format, options)?;
        drain_push(&mut parser, input, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

fn run_push_static<F: StaticFormat>(
    input: &[u8],
    options: ParseOptions,
    want_headers: bool,
) -> FrontResult {
    (|| {
        let mut parser = PushParser::<F>::new(options)?;
        drain_push(&mut parser, input, want_headers)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

// ── Assertions shared by every generic (non-poisoning) case ────────────────

/// Run a case through all three dynamic front ends and, on success, an
/// emitter round trip.
fn check_dynamic(
    input: &[u8],
    format: FormatOptions,
    options: &ParseOptions,
    want_headers: bool,
    expected: &FrontResult,
) {
    assert_eq!(
        &run_slice_dynamic(input, format, options.clone(), want_headers),
        expected,
        "SliceParser (dynamic) disagreed"
    );
    assert_eq!(
        &run_io_dynamic(input, format, options.clone(), want_headers),
        expected,
        "short-reading IoParser (dynamic) disagreed"
    );
    assert_eq!(
        &run_push_dynamic(input, format, options.clone(), want_headers),
        expected,
        "one-byte PushParser (dynamic) disagreed"
    );
    if let Ok(parsed) = expected {
        assert_roundtrip(format, options.clone(), want_headers, parsed);
    }
}

/// Run a case through all three static front ends for `F`.
fn check_static<F: StaticFormat>(
    input: &[u8],
    options: &ParseOptions,
    want_headers: bool,
    expected: &FrontResult,
) {
    assert_eq!(
        &run_slice_static::<F>(input, options.clone(), want_headers),
        expected,
        "SliceParser (static) disagreed"
    );
    assert_eq!(
        &run_io_static::<F>(input, options.clone(), want_headers),
        expected,
        "short-reading IoParser (static) disagreed"
    );
    assert_eq!(
        &run_push_static::<F>(input, options.clone(), want_headers),
        expected,
        "one-byte PushParser (static) disagreed"
    );
}

/// Re-emit a successfully parsed case through [`VecEmitter`] and reparse it,
/// expecting the identical [`Parsed`] value back.
fn assert_roundtrip(
    format: FormatOptions,
    options: ParseOptions,
    want_headers: bool,
    parsed: &Parsed,
) {
    let mut emitter = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())
        .expect("emit options for a format that already parsed must be valid");
    if let Some(headers) = &parsed.headers {
        let fields: Vec<&[u8]> = headers.iter().map(Vec::as_slice).collect();
        emitter.emit_slices(&fields).expect("emit the header row");
    }
    for row in &parsed.rows {
        let fields: Vec<&[u8]> = row.iter().map(Vec::as_slice).collect();
        emitter.emit_slices(&fields).expect("emit a data row");
    }
    let encoded = emitter.into_inner();
    let reparsed = run_slice_dynamic(&encoded, format, options, want_headers)
        .expect("the round-tripped document must reparse");
    assert_eq!(&reparsed, parsed, "round trip changed the parsed result");
}

// ── Field-count-mismatch "poisoning": two bespoke cases ─────────────────────
//
// Go and rust-csv both treat a wrong-width record as a non-fatal, per-record
// error and keep yielding subsequent records. coseva's parser instead
// poisons the stream: the mismatch is reported once, and every further
// `next_line()` call fails with `ErrorKind::ParserFailed` rather than
// resuming. This is a documented, intentional semantic difference (see
// fixtures/reference/README.md), so these two cases are asserted step by
// step, across all three front ends, rather than through the generic
// single-outcome `check_dynamic`.

#[expect(
    clippy::panic,
    reason = "the panic distinguishes which poisoned-parser outcome was unexpectedly observed"
)]
fn assert_poison_slice<F: CsvFormat>(
    mut parser: SliceParser<'_, F>,
    first_row: &[&[u8]],
    mismatch: ErrorKind,
) {
    let mut line = parser
        .next_line()
        .expect("the first line is reachable")
        .expect("a first record exists");
    let mut record = ByteRecord::new();
    line.read_byte_record_into(&mut record)
        .expect("the first record matches its own field count");
    assert_eq!(record.iter().collect::<Vec<_>>(), first_row);

    let mut second = parser
        .next_line()
        .expect("the second line is reachable")
        .expect("a second record exists");
    let error = second
        .record()
        .expect_err("the second record's field count mismatches the first");
    assert_eq!(error.kind(), mismatch);

    match parser.next_line() {
        Err(error) => assert_eq!(error.kind(), ErrorKind::ParserFailed),
        Ok(Some(_)) => {
            panic!("expected ParserFailed after the poisoned mismatch, got a third record")
        }
        Ok(None) => panic!("expected ParserFailed after the poisoned mismatch, got end of input"),
    }
}

#[expect(
    clippy::panic,
    reason = "the panic distinguishes which poisoned-parser outcome was unexpectedly observed"
)]
fn assert_poison_io<R: Read, F: CsvFormat>(
    mut parser: IoParser<R, F>,
    first_row: &[&[u8]],
    mismatch: ErrorKind,
) {
    let mut line = parser
        .next_line()
        .expect("the first line is reachable")
        .expect("a first record exists");
    let mut record = ByteRecord::new();
    line.read_byte_record_into(&mut record)
        .expect("the first record matches its own field count");
    assert_eq!(record.iter().collect::<Vec<_>>(), first_row);

    let mut second = parser
        .next_line()
        .expect("the second line is reachable")
        .expect("a second record exists");
    let error = second
        .record()
        .expect_err("the second record's field count mismatches the first");
    assert_eq!(error.kind(), mismatch);

    match parser.next_line() {
        Err(error) => assert_eq!(error.kind(), ErrorKind::ParserFailed),
        Ok(Some(_)) => {
            panic!("expected ParserFailed after the poisoned mismatch, got a third record")
        }
        Ok(None) => panic!("expected ParserFailed after the poisoned mismatch, got end of input"),
    }
}

/// Fed one byte at a time. The first record completes (and is verified) as
/// soon as its own terminator is fed; the second record, missing its own
/// terminator, only surfaces once `finish()` signals end of input, matching
/// how a caller would actually observe this sequence from a byte stream.
#[expect(
    clippy::panic,
    reason = "the panic distinguishes which poisoned-parser outcome was unexpectedly observed"
)]
fn assert_poison_push(input: &[u8], first_row: &[&[u8]], mismatch: ErrorKind) {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::MatchFirst),
    )
    .expect("valid parser");

    let mut rows: Vec<Vec<Vec<u8>>> = Vec::new();
    for byte in input.chunks(1) {
        let mut chunk = parser.chunk(byte);
        while let Ok(Some(mut line)) = chunk.next_line() {
            let record = line
                .record()
                .expect("the first record matches its own field count");
            rows.push(record.iter().map(<[u8]>::to_vec).collect());
        }
        let done = chunk.done();
        assert_eq!(
            done,
            byte.len(),
            "a single fed byte is always fully absorbed"
        );
    }
    let expected_first: Vec<Vec<u8>> = first_row.iter().map(|f| f.to_vec()).collect();
    assert_eq!(rows, vec![expected_first]);

    parser.finish();
    let mut chunk = parser.chunk(b"");
    let mut line = chunk
        .next_line()
        .expect("the second record's line is reachable")
        .expect("a second record exists");
    let error = line
        .record()
        .expect_err("the second record's field count mismatches the first");
    assert_eq!(error.kind(), mismatch);

    match chunk.next_line() {
        Err(error) => assert_eq!(error.kind(), ErrorKind::ParserFailed),
        other => panic!("expected ParserFailed after the poisoned mismatch, got {other:?}"),
    }
}

// ── go/* cases: Go standard library `encoding/csv`, BSD-3-Clause ───────────
// Upstream: src/encoding/csv/reader_test.go, `readTests` table, commit
// 9549c91031e12f484d5b02d4531f9022231f951f. Full provenance per case in
// fixtures/reference/manifest.json (id `go/<name>`).

/// `go/simple` ("Simple"): one plain record.
#[test]
fn go_simple() {
    let input = b"a,b,c\n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/crlf` ("CRLF"): CRLF record terminators.
#[test]
fn go_crlf() {
    let input = b"a,b\r\nc,d\r\n";
    let expected = ok_case(None, &[&[b"a", b"b"], &[b"c", b"d"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/bare_cr` ("BareCR"): a bare CR inside an unquoted field is data, not a
/// terminator; only the trailing CRLF ends the record.
#[test]
fn go_bare_cr() {
    let input = b"a,b\rc,d\r\n";
    let expected = ok_case(None, &[&[b"a", b"b\rc", b"d"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/rfc4180_test` ("RFC4180test"): the RFC 4180 illustrative example,
/// including a quoted embedded newline and an escaped quote.
#[test]
fn go_rfc4180_test() {
    let input =
        b"#field1,field2,field3\n\"aaa\",\"bb\nb\",\"ccc\"\n\"a,a\",\"b\"\"bb\",\"ccc\"\nzzz,yyy,xxx\n";
    let expected = ok_case(
        None,
        &[
            &[b"#field1", b"field2", b"field3"],
            &[b"aaa", b"bb\nb", b"ccc"],
            &[b"a,a", b"b\"bb", b"ccc"],
            &[b"zzz", b"yyy", b"xxx"],
        ],
    );
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/no_eol` ("NoEOLTest"): no trailing record terminator at all.
#[test]
fn go_no_eol() {
    let input = b"a,b,c";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/semicolon` ("Semicolon"): a semicolon delimiter.
#[test]
fn go_semicolon() {
    let input = b"a;b;c\n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_SEMICOLON, &opts_none(), false, &expected);
    check_static::<Semicolon>(input, &opts_none(), false, &expected);
}

/// `go/multiline` ("MultiLine"): every field quoted and embedding newlines.
#[test]
fn go_multiline() {
    let input = b"\"two\nline\",\"one line\",\"three\nline\nfield\"";
    let expected = ok_case(None, &[&[b"two\nline", b"one line", b"three\nline\nfield"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/blank_line` ("BlankLine"): physical blank lines between records are
/// skipped under `BlankRecords::Skip`.
#[test]
fn go_blank_line() {
    let input = b"a,b,c\n\nd,e,f\n\n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"], &[b"d", b"e", b"f"]]);
    check_dynamic(input, FMT_BLANK_SKIP, &opts_none(), false, &expected);
}

/// `go/blank_line_field_count` ("BlankLineFieldCount"): as above, plus
/// `FieldCount::MatchFirst`; both records already have three fields, so the
/// check never trips.
#[test]
fn go_blank_line_field_count() {
    let input = b"a,b,c\n\nd,e,f\n\n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"], &[b"d", b"e", b"f"]]);
    check_dynamic(
        input,
        FMT_BLANK_SKIP,
        &opts_none_match_first(),
        false,
        &expected,
    );
}

/// `go/trim_space` ("TrimSpace"): leading spaces trimmed. Go's
/// `TrimLeadingSpace` trims only the leading edge; coseva's `Whitespace::ALL`
/// trims both. This input has no trailing whitespace on any field, so the
/// two policies coincide here -- see `fixtures/reference/README.md` for why
/// that is not a general equivalence.
#[test]
fn go_trim_space() {
    let input = b" a,  b,   c\n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_TRIM_ALL, &opts_none(), false, &expected);
    check_static::<TrimmedCsv>(input, &opts_none(), false, &expected);
}

/// `go/leading_space` ("LeadingSpace"): the same bytes as `go/trim_space`, with
/// no trimming configured, so the leading spaces are retained.
#[test]
fn go_leading_space() {
    let input = b" a,  b,   c\n";
    let expected = ok_case(None, &[&[b" a", b"  b", b"   c"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/comment` ("Comment"): `#`-prefixed physical lines are comments, skipped
/// along with blank lines under `CommentedCsv`.
#[test]
fn go_comment() {
    let input = b"#1,2,3\na,b,c\n#comment";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_COMMENTED, &opts_none(), false, &expected);
    check_static::<CommentedCsv>(input, &opts_none(), false, &expected);
}

/// `go/no_comment` ("NoComment"): the same leading `#`-line with no comment
/// byte configured, so it is an ordinary data record.
#[test]
fn go_no_comment() {
    let input = b"#1,2,3\na,b,c";
    let expected = ok_case(None, &[&[b"#1", b"2", b"3"], &[b"a", b"b", b"c"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/bad_double_quotes` ("BadDoubleQuotes"): a quote appears mid-field,
/// outside the start of a quoted field.
#[test]
fn go_bad_double_quotes() {
    let input = b"a\"\"b,c";
    let expected = Err(ErrorKind::UnexpectedQuote);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/bad_bare_quote` ("BadBareQuote"): a bare quote follows unquoted content.
#[test]
fn go_bad_bare_quote() {
    let input = b"a \"word\",\"b\"";
    let expected = Err(ErrorKind::UnexpectedQuote);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/bad_trailing_quote` ("BadTrailingQuote"): a bare quote follows unquoted
/// content at the end of a record.
#[test]
fn go_bad_trailing_quote() {
    let input = b"\"a word\",b\"";
    let expected = Err(ErrorKind::UnexpectedQuote);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/extraneous_quote` ("ExtraneousQuote"): unquoted content follows a
/// properly closed quoted field.
#[test]
fn go_extraneous_quote() {
    let input = b"\"a \"word\",\"b\"";
    let expected = Err(ErrorKind::UnexpectedByteAfterQuote(b'w'));
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/start_line1` ("`StartLine1`", Go issue 19019): a quoted field spanning a
/// newline, followed directly by unquoted content.
#[test]
fn go_start_line1() {
    let input = b"a,\"b\nc\"d,e";
    let expected = Err(ErrorKind::UnexpectedByteAfterQuote(b'd'));
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/bad_field_count` ("BadFieldCount"): a second record with fewer fields
/// than the first, under `FieldCount::MatchFirst`. Go treats this as a
/// non-fatal per-record error and keeps yielding records; coseva poisons the
/// stream instead. See the `assert_poison_*` helpers and
/// `fixtures/reference/README.md`.
#[test]
fn go_bad_field_count() {
    let input: &[u8] = b"a,b,c\nd,e";
    let mismatch = ErrorKind::FieldCountMismatch {
        expected: 3,
        actual: 2,
    };

    let slice = SliceParser::with_options(input, FMT_DEFAULT, opts_none_match_first())
        .expect("valid parser");
    assert_poison_slice(slice, &[b"a", b"b", b"c"], mismatch);

    let io = IoParser::with_options(
        ShortReader::new(input),
        FMT_DEFAULT,
        opts_none_match_first(),
    )
    .expect("valid parser");
    assert_poison_io(io, &[b"a", b"b", b"c"], mismatch);

    assert_poison_push(input, &[b"a", b"b", b"c"], mismatch);
}

/// `go/field_count` ("FieldCount"): a second record with fewer fields under
/// the default flexible field count, which permits it.
#[test]
fn go_field_count() {
    let input = b"a,b,c\nd,e";
    let expected = ok_case(None, &[&[b"a", b"b", b"c"], &[b"d", b"e"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/trailing_comma_eof` ("TrailingCommaEOF"): a trailing delimiter with no
/// following byte produces a final empty field.
#[test]
fn go_trailing_comma_eof() {
    let input = b"a,b,c,";
    let expected = ok_case(None, &[&[b"a", b"b", b"c", b""]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/trailing_comma_space_eof` ("TrailingCommaSpaceEOF"): a trailing space
/// after the last delimiter is trimmed away, leaving an empty field.
#[test]
fn go_trailing_comma_space_eof() {
    let input = b"a,b,c, ";
    let expected = ok_case(None, &[&[b"a", b"b", b"c", b""]]);
    check_dynamic(input, FMT_TRIM_ALL, &opts_none(), false, &expected);
    check_static::<TrimmedCsv>(input, &opts_none(), false, &expected);
}

/// `go/trailing_comma_line3` ("TrailingCommaLine3"): the same trailing-comma
/// shape on the third record of a multi-record document.
#[test]
fn go_trailing_comma_line3() {
    let input = b"a,b,c\nd,e,f\ng,hi,";
    let expected = ok_case(
        None,
        &[
            &[b"a", b"b", b"c"],
            &[b"d", b"e", b"f"],
            &[b"g", b"hi", b""],
        ],
    );
    check_dynamic(input, FMT_TRIM_ALL, &opts_none(), false, &expected);
    check_static::<TrimmedCsv>(input, &opts_none(), false, &expected);
}

/// `go/not_trailing_comma3` ("NotTrailingComma3"): a trailing field that is a
/// single space, with no trimming configured, so it is retained.
#[test]
fn go_not_trailing_comma3() {
    let input = b"a,b,c, \n";
    let expected = ok_case(None, &[&[b"a", b"b", b"c", b" "]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/comma_field_test` ("CommaFieldTest"): a battery of field-count and
/// empty-field combinations, unquoted and quoted.
#[test]
fn go_comma_field_test() {
    let input = b"x,y,z,w\nx,y,z,\nx,y,,\nx,,,\n,,,\n\"x\",\"y\",\"z\",\"w\"\n\"x\",\"y\",\"z\",\"\"\n\"x\",\"y\",\"\",\"\"\n\"x\",\"\",\"\",\"\"\n\"\",\"\",\"\",\"\"\n";
    let expected = ok_case(
        None,
        &[
            &[b"x", b"y", b"z", b"w"],
            &[b"x", b"y", b"z", b""],
            &[b"x", b"y", b"", b""],
            &[b"x", b"", b"", b""],
            &[b"", b"", b"", b""],
            &[b"x", b"y", b"z", b"w"],
            &[b"x", b"y", b"z", b""],
            &[b"x", b"y", b"", b""],
            &[b"x", b"", b"", b""],
            &[b"", b"", b"", b""],
        ],
    );
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/trailing_comma_ineffective1` ("TrailingCommaIneffective1"): trimming is
/// configured but the trailing field on the first record is already empty.
#[test]
fn go_trailing_comma_ineffective1() {
    let input = b"a,b,\nc,d,e";
    let expected = ok_case(None, &[&[b"a", b"b", b""], &[b"c", b"d", b"e"]]);
    check_dynamic(input, FMT_TRIM_ALL, &opts_none(), false, &expected);
    check_static::<TrimmedCsv>(input, &opts_none(), false, &expected);
}

/// `go/even_quotes` ("EvenQuotes"): eight quote characters resolve to three
/// literal quotes in one field.
#[test]
fn go_even_quotes() {
    let input = b"\"\"\"\"\"\"\"\"";
    let expected = ok_case(None, &[&[b"\"\"\""]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/odd_quotes` ("OddQuotes"): seven quote characters leave a quoted field
/// unterminated at EOF. Go reports this as `ErrQuote`; coseva's matching
/// rejection is the more specific `UnterminatedQuotedField`.
#[test]
fn go_odd_quotes() {
    let input = b"\"\"\"\"\"\"\"";
    let expected = Err(ErrorKind::UnterminatedQuotedField);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/crlf_in_quoted_field` ("`CRLFInQuotedField`", Go issue 21201): an embedded
/// CRLF inside a quoted field. Go normalizes it to a bare LF; coseva returns
/// the bytes unchanged -- a documented semantic difference.
#[test]
fn go_crlf_in_quoted_field() {
    let input = b"A,\"Hello\r\nHi\",B\r\n";
    let expected = ok_case(None, &[&[b"A", b"Hello\r\nHi", b"B"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/trailing_cr` ("TrailingCR"): a bare CR at end of input with no
/// following LF. Go strips it as an implicit terminator; coseva's
/// `RecordEnding::Newline` only strips a CR immediately followed by an LF, so
/// it is retained as data here -- a documented semantic difference.
#[test]
fn go_trailing_cr() {
    let input = b"field1,field2\r";
    let expected = ok_case(None, &[&[b"field1", b"field2\r"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/quoted_trailing_cr` ("QuotedTrailingCR"): a bare trailing CR immediately
/// after a closing quote at EOF. Go accepts and strips it; coseva rejects the
/// CR as an unexpected byte after the closing quote -- a documented semantic
/// difference.
#[test]
fn go_quoted_trailing_cr() {
    let input = b"\"field\"\r";
    let expected = Err(ErrorKind::UnexpectedByteAfterQuote(b'\r'));
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/quoted_trailing_cr_cr` ("QuotedTrailingCRCR"): two bare trailing CRs
/// after a closing quote. Both Go (`ErrQuote`) and coseva
/// (`UnexpectedByteAfterQuote`) reject this input, for differently named
/// reasons -- a genuine cross-implementation agreement, not a semantic
/// difference.
#[test]
fn go_quoted_trailing_cr_cr() {
    let input = b"\"field\"\r\r";
    let expected = Err(ErrorKind::UnexpectedByteAfterQuote(b'\r'));
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `go/config_bad_comma_newline` ("BadComma1"): a delimiter equal to the
/// record terminator is rejected. Go validates this lazily on first `Read()`;
/// coseva validates eagerly at construction.
#[test]
fn go_config_bad_comma_newline() {
    let expected = Err(ErrorKind::Configuration);
    check_dynamic(
        b"",
        FMT_DELIMITER_IS_NEWLINE,
        &opts_none(),
        false,
        &expected,
    );
}

/// `go/config_comment_equals_delimiter` ("BadCommaComment"): a comment byte
/// equal to the delimiter is rejected, eagerly at construction in coseva
/// (Go's analogous case uses `Comma == Comment == 'X'`; this uses the
/// default comma delimiter colliding with a comment of `,` instead, which
/// exercises the same rejection).
#[test]
fn go_config_comment_equals_delimiter() {
    let expected = Err(ErrorKind::Configuration);
    check_dynamic(
        b"",
        FMT_COMMENT_EQ_DELIMITER,
        &opts_none(),
        false,
        &expected,
    );
}

// ── rust_csv/* cases: BurntSushi/rust-csv, Unlicense ────────────────────────
// Upstream: src/reader.rs, `mod tests`, commit
// 05612e87e6e92910d40d7214b70952b8434fef9b. Full provenance per case in
// fixtures/reference/manifest.json (id `rust_csv/<name>`).

/// `rust_csv/read_byte_record:` a quoted field containing the delimiter.
#[test]
fn rust_csv_read_byte_record() {
    let input = b"foo,\"b,ar\",baz\nabc,mno,xyz";
    let expected = ok_case(
        None,
        &[&[b"foo", b"b,ar", b"baz"], &[b"abc", b"mno", b"xyz"]],
    );
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `rust_csv/read_trimmed_records_and_headers:` `Trim::All` (coseva's
/// `Whitespace::ALL`) trims both headers and data fields, quoted or not.
#[test]
fn rust_csv_read_trimmed_records_and_headers() {
    let input = b"foo,  bar,\tbaz\n  1,  2,  3\n1\t,\t,3\t\t";
    let expected = ok_case(
        Some(&[b"foo", b"bar", b"baz"]),
        &[&[b"1", b"2", b"3"], &[b"1", b"", b"3"]],
    );
    check_dynamic(input, FMT_TRIM_ALL, &opts_headers(), true, &expected);
    check_static::<TrimmedCsv>(input, &opts_headers(), true, &expected);
}

/// `rust_csv/read_trimmed_header:` `Trim::Headers` (coseva's
/// `Whitespace::HEADERS`) trims only the header row, not data fields.
/// Truncated to the header plus the single data row upstream's test actually
/// reads (its second data row is present in the upstream local but never
/// read by that test).
#[test]
fn rust_csv_read_trimmed_header() {
    let input = b"foo,  bar,\tbaz\n  1,  2,  3\n";
    let expected = ok_case(
        Some(&[b"foo", b"bar", b"baz"]),
        &[&[b"  1", b"  2", b"  3"]],
    );
    check_dynamic(input, FMT_TRIM_HEADERS, &opts_headers(), true, &expected);
}

/// `rust_csv/read_trimmed_records:` `Trim::Fields` (coseva's
/// `Whitespace::FIELDS`) trims only data fields, not the header row.
/// Truncated the same way as `rust_csv/read_trimmed_header` above.
#[test]
fn rust_csv_read_trimmed_records() {
    let input = b"foo,  bar,\tbaz\n  1,  2,  3\n";
    let expected = ok_case(Some(&[b"foo", b"  bar", b"\tbaz"]), &[&[b"1", b"2", b"3"]]);
    check_dynamic(input, FMT_TRIM_FIELDS, &opts_headers(), true, &expected);
}

/// `rust_csv/read_trimmed_records_without_headers:` `Trim::All` with no header
/// row configured.
#[test]
fn rust_csv_read_trimmed_records_without_headers() {
    let input = b"a1, b1\t,\t c1\t\n";
    let expected = ok_case(None, &[&[b"a1", b"b1", b"c1"]]);
    check_dynamic(input, FMT_TRIM_ALL, &opts_none(), false, &expected);
    check_static::<TrimmedCsv>(input, &opts_none(), false, &expected);
}

/// `rust_csv/read_record_unequal_fails:` a second record with more fields than
/// the first, under rust-csv's default `flexible(false)` (coseva's
/// `FieldCount::MatchFirst`). rust-csv reports `ErrorKind::UnequalLengths`
/// as a per-record error and can keep reading; coseva poisons the stream.
/// See the `assert_poison_*` helpers and `fixtures/reference/README.md`.
#[test]
fn rust_csv_read_record_unequal_fails() {
    let input: &[u8] = b"foo\nbar,baz";
    let mismatch = ErrorKind::FieldCountMismatch {
        expected: 1,
        actual: 2,
    };

    let slice = SliceParser::with_options(input, FMT_DEFAULT, opts_none_match_first())
        .expect("valid parser");
    assert_poison_slice(slice, &[b"foo"], mismatch);

    let io = IoParser::with_options(
        ShortReader::new(input),
        FMT_DEFAULT,
        opts_none_match_first(),
    )
    .expect("valid parser");
    assert_poison_io(io, &[b"foo"], mismatch);

    assert_poison_push(input, &[b"foo"], mismatch);
}

/// `rust_csv/read_record_unequal_ok:` the same shape as
/// `rust_csv/read_record_unequal_fails`, but under rust-csv's
/// `flexible(true)` (coseva's default `FieldCount::Flexible`), which permits
/// it.
#[test]
fn rust_csv_read_record_unequal_ok() {
    let input = b"foo\nbar,baz";
    let expected = ok_case(None, &[&[b"foo"], &[b"bar", b"baz"]]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `rust_csv/headers_on_empty_data:` reading headers on empty input.
/// rust-csv's `byte_headers()` returns an empty-but-present zero-field
/// record; coseva's `headers()` returns `Ok(None)`, since there is no first
/// line to promote -- a documented semantic difference asserting coseva's
/// actual behavior.
#[test]
fn rust_csv_headers_on_empty_data() {
    let input: &[u8] = b"";
    let expected = ok_case(None, &[]);
    check_dynamic(input, FMT_DEFAULT, &opts_headers(), true, &expected);
    check_static::<Csv>(input, &opts_headers(), true, &expected);
}

/// `rust_csv/no_headers_on_empty_data:` reading records on empty input with no
/// header row configured yields no records.
#[test]
fn rust_csv_no_headers_on_empty_data() {
    let input: &[u8] = b"";
    let expected = ok_case(None, &[]);
    check_dynamic(input, FMT_DEFAULT, &opts_none(), false, &expected);
    check_static::<Csv>(input, &opts_none(), false, &expected);
}

/// `rust_csv/header_invalid_utf8:` a header record containing an invalid UTF-8
/// byte. coseva's `headers()` always returns byte-level headers successfully
/// -- there is no eager string-header conversion to fail the way rust-csv's
/// `headers()` does. The same UTF-8 failure rust-csv reports from
/// `headers()` is instead observed here by explicitly converting the byte
/// header record with `TextRecord::try_from`.
#[test]
fn rust_csv_header_invalid_utf8() {
    let input: &[u8] = b"foo,b\xFFar,baz\na,b,c\nd,e,f";
    let expected = ok_case(
        Some(&[b"foo", b"b\xFFar", b"baz"]),
        &[&[b"a", b"b", b"c"], &[b"d", b"e", b"f"]],
    );
    check_dynamic(input, FMT_DEFAULT, &opts_headers(), true, &expected);
    check_static::<Csv>(input, &opts_headers(), true, &expected);

    let mut parser =
        SliceParser::with_options(input, FMT_DEFAULT, ParseOptions::new()).expect("valid parser");
    let headers = parser
        .headers()
        .expect("headers() itself never validates UTF-8")
        .expect("a header row exists")
        .clone();
    let error = TextRecord::try_from(&headers)
        .expect_err("promoting an invalid-UTF-8 header record to text must fail");
    assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)));
}

/// `rust_csv/read_record_headers:` a plain header row plus two data records.
#[test]
fn rust_csv_read_record_headers() {
    let input = b"foo,bar,baz\na,b,c\nd,e,f";
    let expected = ok_case(
        Some(&[b"foo", b"bar", b"baz"]),
        &[&[b"a", b"b", b"c"], &[b"d", b"e", b"f"]],
    );
    check_dynamic(input, FMT_DEFAULT, &opts_headers(), true, &expected);
    check_static::<Csv>(input, &opts_headers(), true, &expected);
}

// ── Manifest/test consistency ───────────────────────────────────────────────

/// Every case id declared in `fixtures/reference/manifest.json`, so a case
/// added or removed from one without the other fails loudly.
const CASE_IDS: &[&str] = &[
    "go/simple",
    "go/crlf",
    "go/bare_cr",
    "go/rfc4180_test",
    "go/no_eol",
    "go/semicolon",
    "go/multiline",
    "go/blank_line",
    "go/blank_line_field_count",
    "go/trim_space",
    "go/leading_space",
    "go/comment",
    "go/no_comment",
    "go/bad_double_quotes",
    "go/bad_bare_quote",
    "go/bad_trailing_quote",
    "go/extraneous_quote",
    "go/start_line1",
    "go/bad_field_count",
    "go/field_count",
    "go/trailing_comma_eof",
    "go/trailing_comma_space_eof",
    "go/trailing_comma_line3",
    "go/not_trailing_comma3",
    "go/comma_field_test",
    "go/trailing_comma_ineffective1",
    "go/even_quotes",
    "go/odd_quotes",
    "go/crlf_in_quoted_field",
    "go/trailing_cr",
    "go/quoted_trailing_cr",
    "go/quoted_trailing_cr_cr",
    "go/config_bad_comma_newline",
    "go/config_comment_equals_delimiter",
    "rust_csv/read_byte_record",
    "rust_csv/read_trimmed_records_and_headers",
    "rust_csv/read_trimmed_header",
    "rust_csv/read_trimmed_records",
    "rust_csv/read_trimmed_records_without_headers",
    "rust_csv/read_record_unequal_fails",
    "rust_csv/read_record_unequal_ok",
    "rust_csv/headers_on_empty_data",
    "rust_csv/no_headers_on_empty_data",
    "rust_csv/header_invalid_utf8",
    "rust_csv/read_record_headers",
];

/// The manifest and this file must agree on exactly which cases exist.
#[test]
fn manifest_matches_this_file() {
    let manifest = include_str!("fixtures/reference/manifest.json");
    for id in CASE_IDS {
        let needle = format!("\"id\": \"{id}\"");
        assert!(
            manifest.contains(&needle),
            "manifest.json is missing case {id}, or its formatting changed"
        );
    }
    let manifest_case_count = manifest.matches("\"id\":").count();
    assert_eq!(
        manifest_case_count,
        CASE_IDS.len(),
        "manifest.json's case count drifted from this file's CASE_IDS list"
    );
}
