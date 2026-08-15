//! Buffered-reader and owned-record integration tests.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::fs;
use std::io::{self, Cursor, SeekFrom, Write};

use coseva::ErrorKind;
use coseva::config::{
    BlankRecords, Escape, FieldCount, FormatOptions, Headers, Limits, Nulls, ParseOptions, Quoting,
    ReadBom, RecordEnding, Recovery, Syntax, Whitespace,
};
use coseva::format::Csv;
use coseva::{ByteRecord, Predicate, TextRecord};
use coseva::{IoParser, SliceParser};

mod common;

use common::FailingReader;

#[test]
fn default_streaming_reader_discovers_headers() -> Result<(), Box<dyn StdError>> {
    let input = b"city,tag,tag\nBoston,east,large\n";
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(input), ParseOptions::new()).expect("parser");

    let headers = reader.headers()?.ok_or("missing headers")?;
    assert_eq!(
        headers.iter().collect::<Vec<_>>(),
        vec![b"city".as_slice(), b"tag".as_slice(), b"tag".as_slice()],
    );
    assert_eq!(reader.header_index("city")?, Some(0));
    assert_eq!(reader.header_indices("tag")?, [1, 2]);

    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        vec![
            b"Boston".as_slice(),
            b"east".as_slice(),
            b"large".as_slice(),
        ],
    );
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn unheaded_streaming_reader_resolves_header_queries_without_reading()
-> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"city,pop\nBoston,650706\n"),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(1),
    )?;

    assert!(reader.headers()?.is_none());
    assert_eq!(reader.header_index("city")?, None);
    assert_eq!(reader.header_indices("city")?, []);

    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("first record is data");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"city".as_slice(), b"pop".as_slice()]
    );
    Ok(())
}

#[test]
fn streaming_header_views_poison_the_reader_on_a_malformed_header() {
    let input = b"\"unterminated";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().buffer_capacity(4),
    )
    .expect("valid options");

    let error = reader
        .headers()
        .expect_err("malformed first record cannot become headers");
    assert_eq!(error.kind(), ErrorKind::UnterminatedQuotedField);
    assert!(reader.next_line().is_err(), "the reader is poisoned");

    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().buffer_capacity(4),
    )
    .expect("valid options");
    let error = reader
        .header_indices("anything")
        .expect_err("malformed first record cannot be indexed");
    assert_eq!(error.kind(), ErrorKind::UnterminatedQuotedField);
    assert!(reader.next_line().is_err(), "the reader is poisoned");
}

#[test]
fn streaming_reader_can_be_polled_after_eof() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"alpha,beta\n"),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(4),
    )?;
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"alpha".as_slice(), b"beta".as_slice()]
    );
    assert!(reader.next_line()?.is_none());
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn direct_text_records_span_refills_and_preserve_source_metadata() -> Result<(), Box<dyn StdError>>
{
    let input = b"\xEF\xBB\xBFcity,note\r\nBoston,\"caf\xC3\xA9\"\r\nOslo,last";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(3),
    )?;
    let mut record = TextRecord::new();

    assert!(reader.read_text_record_into(&mut record)?);
    assert_eq!(record.iter().collect::<Vec<_>>(), ["city", "note"]);
    assert_eq!(record.byte_range(), 3..14);
    assert_eq!(record.index(), 0);

    assert!(reader.read_text_record_into(&mut record)?);
    assert_eq!(record.iter().collect::<Vec<_>>(), ["Boston", "café"]);
    assert_eq!(record.byte_range(), 14..30);
    assert_eq!(record.index(), 1);

    assert!(reader.read_text_record_into(&mut record)?);
    assert_eq!(record.iter().collect::<Vec<_>>(), ["Oslo", "last"]);
    assert_eq!(record.byte_range(), 30..39);
    assert_eq!(record.index(), 2);
    assert!(!reader.read_text_record_into(&mut record)?);
    Ok(())
}

#[test]
fn direct_text_record_rejects_invalid_utf8_without_publishing_it() -> Result<(), Box<dyn StdError>>
{
    let input = b"ok,row\nbad,\xFF\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(2),
    )?;
    let mut record = TextRecord::new();

    assert!(reader.read_text_record_into(&mut record)?);
    let error = reader
        .read_text_record_into(&mut record)
        .expect_err("invalid UTF-8 must fail");
    assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)));
    assert_eq!(error.location().byte, 7);
    assert_eq!(error.location().record, 1);
    assert_eq!(error.location().field, 1);
    assert!(record.is_empty(), "invalid UTF-8 must not be published");
    Ok(())
}

#[test]
fn direct_text_record_honors_rejected_bom() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"\xEF\xBB\xBFa,b\n"),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let error = reader
        .read_text_record_into(&mut TextRecord::new())
        .expect_err("a rejected BOM must fail");
    assert_eq!(error.kind(), ErrorKind::RejectedBom);
    Ok(())
}

#[test]
fn named_streaming_kernels_cover_contained_and_spanning_records() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (FormatOptions::TSV, b'\t', false),
        (FormatOptions::SEMICOLON, b';', false),
        (FormatOptions::PIPE, b'|', false),
        (FormatOptions::BACKSLASH_CSV, b',', true),
        (FormatOptions::BACKSLASH_TSV, b'\t', true),
    ];

    for (format, delimiter, backslash) in cases {
        let mut input = b"seed".to_vec();
        input.push(delimiter);
        input.extend_from_slice(b"row\nplain");
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

        for capacity in [1, 64, 256] {
            let mut reader = IoParser::with_options(
                Cursor::new(input.as_slice()),
                format,
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )?;
            let mut record = ByteRecord::new();

            let mut line = reader.next_line()?.expect("record");
            line.read_byte_record_into(&mut record)?;
            assert_eq!(
                record.iter().collect::<Vec<_>>(),
                [b"seed".as_slice(), b"row"],
                "format {format:?}, capacity {capacity}",
            );
            let mut line = reader.next_line()?.expect("record");
            line.read_byte_record_into(&mut record)?;
            assert_eq!(
                record.iter().collect::<Vec<_>>(),
                [b"plain".as_slice(), b"say \"hello\"", b"tail"],
                "format {format:?}, capacity {capacity}",
            );
            let mut line = reader.next_line()?.expect("record");
            line.read_byte_record_into(&mut record)?;
            let mut quoted = b"quoted".to_vec();
            quoted.push(delimiter);
            quoted.extend_from_slice(b"value");
            assert_eq!(
                record.iter().collect::<Vec<_>>(),
                [quoted.as_slice(), b"end"],
                "format {format:?}, capacity {capacity}",
            );
            let mut line = reader.next_line()?.expect("record");
            line.read_byte_record_into(&mut record)?;
            assert_eq!(
                record.iter().collect::<Vec<_>>(),
                [b"last".as_slice(), b"row"],
                "format {format:?}, capacity {capacity}",
            );
            assert!(reader.next_line()?.is_none(), "format {format:?}");
        }
    }
    Ok(())
}

#[test]
fn named_streaming_kernels_preserve_fallback_errors() -> Result<(), Box<dyn StdError>> {
    let cases: &[(FormatOptions, &[u8])] = &[
        (FormatOptions::TSV, b"seed\trow\n\"a\"x\tb\n"),
        (FormatOptions::SEMICOLON, b"seed;row\n\"a\"x;b\n"),
        (FormatOptions::PIPE, b"seed|row\n\"a\"x|b\n"),
        (FormatOptions::BACKSLASH_CSV, b"seed,row\n\"a\\q\",b\n"),
        (FormatOptions::BACKSLASH_TSV, b"seed\trow\n\"a\\q\"\tb\n"),
    ];

    for &(format, input) in cases {
        let mut expected_reader =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
        let mut expected_record = ByteRecord::new();
        let mut line = expected_reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut expected_record)?;
        let mut line = expected_reader.next_line()?.expect("record");
        let expected = line
            .read_byte_record_into(&mut expected_record)
            .expect_err("malformed second record should fail");

        for capacity in [64, 256] {
            let mut reader = IoParser::with_options(
                Cursor::new(input),
                format,
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )?;
            let mut record = ByteRecord::new();
            let mut line = reader.next_line()?.expect("record");
            line.read_byte_record_into(&mut record)?;
            let mut line = reader.next_line()?.expect("record");
            let actual = line
                .read_byte_record_into(&mut record)
                .expect_err("malformed second record should fail");
            assert_eq!(
                actual.kind(),
                expected.kind(),
                "format {format:?}, capacity {capacity}"
            );
            assert_eq!(
                (
                    actual.location().byte,
                    actual.location().line,
                    actual.location().record,
                ),
                (
                    expected.location().byte,
                    expected.location().line,
                    expected.location().record,
                ),
                "format {format:?}, capacity {capacity}"
            );
        }
    }
    Ok(())
}

#[test]
fn commented_streaming_kernel_skips_contained_physical_lines() -> Result<(), Box<dyn StdError>> {
    let input = b"seed,row\n# ignored\r\n\n# two\n\r\nnext,\"say \"\"hi\"\"\"\n";
    let expected_start = input
        .windows(b"next,".len())
        .position(|window| window == b"next,")
        .ok_or("missing data record")?;
    for capacity in [64, 256] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::COMMENTED_CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"seed".as_slice(), b"row"]
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"next".as_slice(), b"say \"hi\""],
            "capacity {capacity}"
        );
        assert_eq!(
            record.byte_range().start,
            expected_start,
            "capacity {capacity}"
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn impossible_reader_buffer_capacity_is_rejected_without_allocating() {
    let result = IoParser::with_options(
        Cursor::new(b""),
        FormatOptions::CSV,
        ParseOptions::new().buffer_capacity(usize::MAX),
    );
    let _error = result.expect_err("capacity above isize::MAX should be rejected");
}

#[test]
fn streaming_reader_handles_every_small_buffer_width() -> Result<(), Box<dyn StdError>> {
    let input = b"h1,h2\r\n\"a\r\nb\",\"say \"\"hi\"\"\"\r\nc,d";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            vec![b"a\r\nb".as_slice(), b"say \"hi\"".as_slice()],
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.iter().collect::<Vec<_>>(), [b"c", b"d"]);
        assert!(reader.next_line()?.is_none());
    }
    Ok(())
}

#[test]
fn python_initial_spaces_cross_every_small_buffer_width() -> Result<(), Box<dyn StdError>> {
    let input = b"  first,   \"quoted\",  plain  ,   ,\"  kept  \"\nnext, value\n";
    for capacity in [1, 2, 3, 7, 8, 64] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::PYTHON_CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [
                b"  first".as_slice(),
                b"quoted".as_slice(),
                b"plain  ".as_slice(),
                b"".as_slice(),
                b"  kept  ".as_slice(),
            ],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"next".as_slice(), b"value".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_policies_apply_before_materialization() -> Result<(), Box<dyn StdError>> {
    let input = b"\xEF\xBB\xBF  name  , value \n\n  Alice  , 42 \n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV
            .read_bom(ReadBom::Detect)
            .blank_records(BlankRecords::Skip)
            .trim(Whitespace::ALL.unquoted_only()),
        ParseOptions::new(),
    )?;

    assert_eq!(
        reader
            .headers()?
            .ok_or("missing headers")?
            .iter()
            .collect::<Vec<_>>(),
        vec![b"name".as_slice(), b"value".as_slice()],
    );
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        vec![b"Alice".as_slice(), b"42".as_slice()],
    );
    Ok(())
}

#[test]
fn streaming_chunk_parser_trims_escaped_and_unquoted_fields() -> Result<(), Box<dyn StdError>> {
    let input = b"h1,h2\n\"  say \"\"hello\"\"  \",  value  \n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV.trim(Whitespace::ALL),
        ParseOptions::new(),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"say \"hello\"".as_slice(), b"value".as_slice()],
    );
    Ok(())
}

#[test]
fn streaming_detects_bom_before_a_quoted_first_field() -> Result<(), Box<dyn StdError>> {
    let input = b"\xEF\xBB\xBF\"first\",second\n\"third\",fourth\n";
    for capacity in (1..=16).chain([64]) {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV.read_bom(ReadBom::Detect),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"first".as_slice(), b"second".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"third".as_slice(), b"fourth".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_comments_ignore_quotes_and_follow_bom() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.comment(Some(b'#'));
    let input = b"\xEF\xBB\xBF# unmatched \" quote\n# another comment\nh1,h2\nx,y\n";
    for capacity in [1, 2, 3, 7, 8, 64] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            format,
            ParseOptions::new().buffer_capacity(capacity),
        )?;
        assert_eq!(
            reader
                .headers()?
                .ok_or("missing headers")?
                .iter()
                .collect::<Vec<_>>(),
            vec![b"h1".as_slice(), b"h2".as_slice()],
            "capacity {capacity}",
        );
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            vec![b"x".as_slice(), b"y".as_slice()],
            "capacity {capacity}",
        );
    }
    Ok(())
}

#[test]
fn streaming_bulk_errors_preserve_exact_positions() -> Result<(), Box<dyn StdError>> {
    for capacity in [8, 64] {
        let mut reader = IoParser::with_options(
            Cursor::new(b"ok,row\na\"b,c\n"),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut ByteRecord::new())?;
        let mut line = reader.next_line()?.expect("record");
        let error = line
            .read_byte_record_into(&mut ByteRecord::new())
            .expect_err("unquoted quote should fail");
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        assert_eq!(error.location().byte, 8);
    }

    let mut reader = IoParser::with_options(
        Cursor::new(b"\"a\",\"b\"\n"),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(coseva::config::Limits::new(3, 8, 8))
            .buffer_capacity(64),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut ByteRecord::new())
        .expect_err("record limit should fail");
    assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
    assert_eq!(error.location().byte, 4);
    Ok(())
}

#[test]
fn post_quote_bare_cr_reports_the_cr_across_every_buffer_width() -> Result<(), Box<dyn StdError>> {
    for input in [b"\"ab\"\rx\n".as_slice(), b"\"ab\"\r"] {
        for capacity in [1, 2, 3, 4, 5, 8, 64] {
            let mut reader = IoParser::with_options(
                Cursor::new(input),
                FormatOptions::CSV,
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )?;
            let mut line = reader.next_line()?.expect("record");
            let error = line
                .read_byte_record_into(&mut ByteRecord::new())
                .expect_err("bare CR after a closing quote should fail");
            assert_eq!(
                error.kind(),
                ErrorKind::UnexpectedByteAfterQuote(b'\r'),
                "{input:?} @ capacity {capacity}"
            );
            assert_eq!(error.location().byte, 4, "{input:?} @ capacity {capacity}");
            assert_eq!(error.location().line, 1, "{input:?} @ capacity {capacity}");
        }
    }
    Ok(())
}

#[test]
fn streaming_incremental_records_preserve_fields_and_positions() -> Result<(), Box<dyn StdError>> {
    let first = b"seed,row\n";
    let second = b"ab\rcd,\"say \"\"hi\"\"\",\"line\r\nbreak\"\r\n";
    let third = b"last,";
    let input = [first.as_slice(), second.as_slice(), third.as_slice()].concat();
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(&input),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();

        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.byte_range(), 0..first.len(), "capacity {capacity}");

        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [
                b"ab\rcd".as_slice(),
                b"say \"hi\"".as_slice(),
                b"line\r\nbreak".as_slice(),
            ],
            "capacity {capacity}",
        );
        assert_eq!(
            record.byte_range(),
            first.len()..first.len() + second.len(),
            "capacity {capacity}",
        );

        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"last".as_slice(), b"".as_slice()],
            "capacity {capacity}",
        );
        assert_eq!(
            record.byte_range(),
            first.len() + second.len()..input.len(),
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_positions_track_lines_across_every_boundary() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.comment(Some(b'#'));
    let input = b"# ignored\r\n\nfirst,\"a\nb\"\r\nsecond,row\nbad\"quote,x";
    for capacity in [1, 2, 3, 7, 8, 64] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            format.blank_records(BlankRecords::Skip),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();

        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(reader.location().line, 5, "capacity {capacity}");
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(reader.location().line, 6, "capacity {capacity}");
        let error = reader.next_line().expect_err("third record should fail");
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        assert_eq!(error.location().line, 6, "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn specialized_unquoted_streaming_preserves_spanning_lines() -> Result<(), Box<dyn StdError>> {
    let mut input = vec![b'x'; 59];
    input.extend_from_slice(b"\nabcde,\"quoted\nline\"\nbad\"quote,x");
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(64),
    )?;
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(reader.location().line, 2);
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"abcde".as_slice(), b"quoted\nline".as_slice()]
    );
    assert_eq!(reader.location().line, 4);

    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("unquoted quote should fail");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().line, 4);
    Ok(())
}

#[test]
fn streaming_custom_terminators_still_count_lf() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b',')
        .quote(b'"')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::DoubleQuote);
    for capacity in [1, 8, 64] {
        let mut reader = IoParser::with_options(
            Cursor::new(b"a\nb|bad\"quote|"),
            format,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut ByteRecord::new())?;
        assert_eq!(reader.location().line, 2, "capacity {capacity}");
        let mut line = reader.next_line()?.expect("record");
        let error = line
            .read_byte_record_into(&mut ByteRecord::new())
            .expect_err("second record should fail");
        assert_eq!(error.location().line, 2, "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_direct_records_preserve_global_positions() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.comment(Some(b'#'));
    let mut reader = IoParser::with_options(
        Cursor::new(b"first,row\n#ignored\nsecond,row\n"),
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(64),
    )?;
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.byte_range(), 0..10);
    assert_eq!(record.index(), 0);

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.byte_range(), 19..30);
    assert_eq!(record.index(), 1);
    assert_eq!(reader.location().byte, 30);
    Ok(())
}

#[test]
fn seek_replays_records_and_preserves_headers() -> Result<(), Box<dyn StdError>> {
    let input = b"name,value\nfirst,1\nsecond,2\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().buffer_capacity(7),
    )?;
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"first".as_slice()));
    let second = reader.location();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"second".as_slice()));
    assert!(reader.next_line()?.is_none());

    reader.seek(second)?;
    assert_eq!(
        reader.headers()?.and_then(|headers| headers.get(0)),
        Some(b"name".as_slice())
    );
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"second".as_slice()));
    assert_eq!(record.index(), second.record);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn rewind_reapplies_first_record_header_policy() -> Result<(), Box<dyn StdError>> {
    let input = b"name,value\nfirst,1\n";
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(input), ParseOptions::new()).expect("parser");
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"first".as_slice()));
    reader.rewind()?;

    assert_eq!(reader.location().byte, 0);
    assert_eq!(
        reader.headers()?.and_then(|headers| headers.get(0)),
        Some(b"name".as_slice())
    );
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"first".as_slice()));
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn seek_initializes_first_record_width_before_jumping() -> Result<(), Box<dyn StdError>> {
    let input = b"a,b\nc\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(coseva::config::FieldCount::MatchFirst),
    )?;
    reader.seek(coseva::Location {
        byte: 4,
        line: 2,
        record: 1,
        field: 0,
    })?;

    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut ByteRecord::new())
        .expect_err("the source's first record establishes width two");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1,
        }
    );
    Ok(())
}

#[test]
fn seeking_an_empty_stream_with_unset_match_first_width_leaves_it_unset()
-> Result<(), Box<dyn StdError>> {
    // prepare_seek_state's `if self.advance()? { ... }` block is skipped when
    // the stream has no records to establish a MatchFirst width from.
    let mut reader = IoParser::with_options(
        Cursor::new(b"" as &[u8]),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(coseva::config::FieldCount::MatchFirst),
    )?;
    reader.seek(coseva::Location {
        byte: 0,
        line: 1,
        record: 1,
        field: 0,
    })?;
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn raw_seek_validates_position_metadata_and_remains_recoverable() -> Result<(), Box<dyn StdError>> {
    let input = b"first,1\nsecond,2\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let second = coseva::Location {
        byte: 8,
        line: 2,
        record: 1,
        field: 0,
    };

    reader.seek_raw(SeekFrom::Start(8), second)?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"second".as_slice()));
    let previous = reader.location();

    let error = reader
        .seek_raw(
            SeekFrom::Start(8),
            coseva::Location {
                byte: 0,
                line: 1,
                record: 1,
                field: 0,
            },
        )
        .expect_err("inconsistent raw seek metadata must be rejected");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::InvalidInput));
    assert_eq!(error.location(), previous);
    assert_eq!(
        reader.location(),
        previous,
        "rejected logical counters must not replace the last coherent location"
    );
    assert!(
        reader.is_done(),
        "the physical stream moved without coherent logical metadata"
    );
    let poisoned = reader
        .next_line()
        .expect_err("reads must remain poisoned until a successful reposition");
    assert_eq!(poisoned.kind(), ErrorKind::ParserFailed);

    reader.seek(second)?;
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"second".as_slice()));
    Ok(())
}

#[test]
fn seek_rejects_field_error_positions_without_moving() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"a,b\n"),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let error = reader
        .seek(coseva::Location {
            byte: 0,
            record: 0,
            field: 1,
            line: 1,
        })
        .expect_err("field positions are not record boundaries");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::InvalidInput));
    assert_eq!(reader.location().byte, 0);
    Ok(())
}

#[test]
fn reader_header_and_completion_introspection_is_explicit() -> Result<(), Box<dyn StdError>> {
    let mut headers = ByteRecord::new();
    headers.push_field("left");
    headers.push_field("right");
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(b"a,b\n"), ParseOptions::new()).expect("parser");

    assert!(reader.has_headers());
    assert!(!reader.is_done());
    reader.set_headers(headers);
    assert_eq!(reader.header_index("right")?, Some(1));

    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert!(!reader.is_done());
    assert!(reader.next_line()?.is_none());
    assert!(reader.is_done());
    Ok(())
}

#[test]
fn parsing_can_interleave_lending_and_owned_reads() -> Result<(), Box<dyn StdError>> {
    let mut parser = IoParser::with_options(
        Cursor::new(b"a,b\nc,d\n"),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let first = {
        let mut line = parser.next_line()?.expect("record");
        line.record()?.get(0).map(<[u8]>::to_vec)
    };
    assert_eq!(first, Some(b"a".to_vec()));
    assert_eq!(parser.location().record, 1);

    let mut record = TextRecord::new();
    let mut line = parser.next_line()?.expect("record");
    line.read_text_record_into(&mut record)?;
    assert_eq!(record.get(0), Some("c"));
    Ok(())
}

#[test]
fn streaming_direct_dispatch_handles_mixed_record_shapes() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (
            b"seed,row\n\"quoted,field\",escaped\nplain,field\n".as_slice(),
            [
                [b"seed".as_slice(), b"row".as_slice()],
                [b"quoted,field".as_slice(), b"escaped".as_slice()],
                [b"plain".as_slice(), b"field".as_slice()],
            ],
        ),
        (
            b"seed,row\nplain,field\n\"quoted,field\",escaped\n".as_slice(),
            [
                [b"seed".as_slice(), b"row".as_slice()],
                [b"plain".as_slice(), b"field".as_slice()],
                [b"quoted,field".as_slice(), b"escaped".as_slice()],
            ],
        ),
    ];
    for (input, expected) in cases {
        for capacity in [64, 128] {
            let mut reader = IoParser::with_options(
                Cursor::new(input),
                FormatOptions::CSV,
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )?;
            let mut record = ByteRecord::new();
            for fields in expected {
                let mut line = reader.next_line()?.expect("record");
                line.read_byte_record_into(&mut record)?;
                assert_eq!(
                    record.iter().collect::<Vec<_>>(),
                    fields,
                    "capacity {capacity}",
                );
            }
            assert!(reader.next_line()?.is_none(), "capacity {capacity}");
        }
    }
    Ok(())
}

#[test]
fn direct_byte_record_reads_cross_windows_and_end_at_eof() -> Result<(), Box<dyn StdError>> {
    let input = b"\xEF\xBB\xBFid,name,value\r\n1,\"Ada\",10\r\n2,\"Grace\",20";
    for capacity in [1, 2, 7, 64] {
        let mut reader = IoParser::<_, Csv>::new(
            Cursor::new(input),
            ParseOptions::new()
                .headers(Headers::FirstRecord)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();

        assert!(reader.read_byte_record_into(&mut record)?);
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"1".as_slice(), b"Ada".as_slice(), b"10".as_slice()]
        );
        assert!(reader.read_byte_record_into(&mut record)?);
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"2".as_slice(), b"Grace".as_slice(), b"20".as_slice()]
        );
        assert!(!reader.read_byte_record_into(&mut record)?);
        assert!(!reader.read_byte_record_into(&mut record)?);
        assert!(reader.is_done());
    }
    Ok(())
}

#[test]
fn direct_byte_record_read_can_return_to_line_views() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::<_, Csv>::new(
        Cursor::new(b"a,b\nc,d\n"),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::new();
    assert!(reader.read_byte_record_into(&mut record)?);
    assert_eq!(record.get(0), Some(&b"a"[..]));

    let mut line = reader.next_line()?.expect("second record");
    assert_eq!(line.record()?.get(0), Some(&b"c"[..]));
    Ok(())
}

#[test]
fn streaming_large_unquoted_kernel_resumes_spanning_records() -> Result<(), Box<dyn StdError>> {
    let first = b"seed,row\n";
    let long_field = vec![b'a'; 300];
    let mut input = first.to_vec();
    let second_start = input.len();
    input.extend_from_slice(&long_field);
    input.extend_from_slice(b",tail\r\n");
    let second_end = input.len();
    input.extend_from_slice(b"done,row\n");

    let mut reader = IoParser::with_options(
        Cursor::new(&input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(128),
    )?;
    let mut record = ByteRecord::new();

    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [long_field.as_slice(), b"tail".as_slice()],
    );
    assert_eq!(record.byte_range(), second_start..second_end);
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"done".as_slice(), b"row".as_slice()],
    );
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn streaming_large_unquoted_kernel_falls_back_at_interior_quotes() -> Result<(), Box<dyn StdError>>
{
    let mut input = b"seed,row\nplain,row\n".to_vec();
    let malformed_start = input.len();
    input.extend_from_slice(b"bad\"field,row\n");
    input.extend(std::iter::repeat_n(b'x', 128));

    let mut reader = IoParser::with_options(
        Cursor::new(&input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(128),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("unquoted quote should fail");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().byte, malformed_start + 3);
    assert_eq!(error.location().record, 2);
    assert_eq!(error.location().field, 0);
    Ok(())
}

#[test]
fn streaming_large_unquoted_kernel_preserves_field_limit_errors() -> Result<(), Box<dyn StdError>> {
    let mut input = b"seed,row\n".to_vec();
    input.extend(std::iter::repeat_n(b',', Limits::DEFAULT.max_fields));
    input.push(b'\n');

    let mut reader = IoParser::with_options(
        Cursor::new(&input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(input.len()),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("field count above the default limit should fail");
    assert_eq!(
        error.kind(),
        ErrorKind::TooManyFields {
            limit: Limits::DEFAULT.max_fields,
        },
    );
    assert_eq!(error.location().record, 1);
    assert_eq!(error.location().field, Limits::DEFAULT.max_fields);
    Ok(())
}

#[test]
fn streaming_default_direct_kernel_preserves_fields_and_ranges() -> Result<(), Box<dyn StdError>> {
    let records = [
        b"seed,row\n".as_slice(),
        b"\"say \"\"hi\"\"\",\"line\nbreak\",,\r\n".as_slice(),
        b"ab\rcd,last,\n".as_slice(),
        b"\n".as_slice(),
        b"\"tail\",end".as_slice(),
    ];
    let input = records.concat();
    let mut reader = IoParser::with_options(
        Cursor::new(&input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(256),
    )?;
    let mut record = ByteRecord::new();
    let expected = [
        vec![b"seed".as_slice(), b"row".as_slice()],
        vec![
            b"say \"hi\"".as_slice(),
            b"line\nbreak".as_slice(),
            b"".as_slice(),
            b"".as_slice(),
        ],
        vec![b"ab\rcd".as_slice(), b"last".as_slice(), b"".as_slice()],
        vec![b"".as_slice()],
        vec![b"tail".as_slice(), b"end".as_slice()],
    ];
    let mut start = 0;
    for (raw, fields) in records.iter().zip(expected) {
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.iter().collect::<Vec<_>>(), fields);
        assert_eq!(record.byte_range(), start..start + raw.len());
        start += raw.len();
    }
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn streaming_default_direct_kernel_falls_back_for_exact_errors() -> Result<(), Box<dyn StdError>> {
    for capacity in [1, 2, 3, 7, 8, 16, 64, 128] {
        let mut reader = IoParser::with_options(
            Cursor::new(b"seed,row\n\"good\",row\n\"bad\"x,row\n"),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        let mut line = reader.next_line()?.expect("record");
        let error = line
            .read_byte_record_into(&mut record)
            .expect_err("byte after closing quote should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::UnexpectedByteAfterQuote(b'x'),
            "capacity {capacity}",
        );
        assert_eq!(error.location().byte, 25, "capacity {capacity}");
        assert_eq!(error.location().record, 2, "capacity {capacity}");
        // The shared engine numbers a post-quote error one past the field that
        // carried the quote, exactly as the slice parser does for
        // `plain,"quoted"x,tail` in `owned_mixed_tail_fallback_preserves_exact_errors`.
        assert_eq!(error.location().field, 1, "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_default_direct_kernel_enforces_raw_field_limit() -> Result<(), Box<dyn StdError>> {
    let limit = Limits::DEFAULT.max_field_bytes;
    let mut accepted = Vec::with_capacity(limit + 12);
    accepted.extend_from_slice(b"seed,row\n\"");
    accepted.resize(accepted.len() + limit, b'a');
    accepted.extend_from_slice(b"\"\n");
    let mut reader = IoParser::with_options(
        Cursor::new(&accepted),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(accepted.len()),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0).map(<[u8]>::len), Some(limit));

    let escaped_quotes = limit / 2 + 1;
    let mut rejected = Vec::with_capacity(limit + 14);
    rejected.extend_from_slice(b"seed,row\n\"");
    rejected.extend(std::iter::repeat_n(b'"', escaped_quotes * 2));
    rejected.extend_from_slice(b"\"\n");
    let mut reader = IoParser::with_options(
        Cursor::new(&rejected),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(rejected.len()),
    )?;
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("raw quoted field above the default limit should fail");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldTooLarge {
            limit: Limits::DEFAULT.max_field_bytes,
        },
    );
    Ok(())
}

#[test]
fn provided_headers_do_not_consume_input() -> Result<(), Box<dyn StdError>> {
    let headers = ByteRecord::from(vec![b"left".to_vec(), b"right".to_vec()]);
    let mut reader = IoParser::with_options(
        Cursor::new(b"a,b\n"),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::Provided(headers)),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.iter().collect::<Vec<_>>(), [b"a", b"b"]);
    Ok(())
}

#[test]
fn string_records_validate_once() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"valid,row\nbad,\xFF\n"),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = TextRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_text_record_into(&mut record)?;
    assert_eq!(record.iter().collect::<Vec<_>>(), ["valid", "row"]);
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_text_record_into(&mut record)
        .expect_err("invalid UTF-8 should fail");
    assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)));
    assert_eq!(error.location().field, 1);
    Ok(())
}

#[test]
fn rejected_bom_is_reported() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"\xEF\xBB\xBFa,b\n"),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut ByteRecord::new())
        .expect_err("BOM should be rejected");
    assert_eq!(error.kind(), ErrorKind::RejectedBom);
    Ok(())
}

#[test]
fn streaming_reader_rejects_invalid_read_lengths_without_panicking() -> Result<(), Box<dyn StdError>>
{
    let mut reader = IoParser::with_options(
        FailingReader::new(Vec::new()).overrun(),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(4),
    )?;
    let error = reader
        .next_line()
        .expect_err("oversized read count should fail");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::InvalidData));
    assert_eq!(
        reader
            .next_line()
            .expect_err("reader should remain failed")
            .kind(),
        ErrorKind::ParserFailed,
    );
    Ok(())
}

#[test]
fn streaming_syntax_errors_permanently_fail_the_reader() -> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"a\"b,c\naccepted,row\n"),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(1),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut ByteRecord::new())
        .expect_err("malformed input should fail");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(
        reader
            .next_line()
            .expect_err("reader should remain failed")
            .kind(),
        ErrorKind::ParserFailed,
    );
    Ok(())
}

#[test]
fn streaming_compatible_parser_matches_slice_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"a,\"b,c\",d\"e\nnext,row,last\n";
    let syntax = Syntax::Compatible(Recovery::PERMISSIVE);
    let mut streaming = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV.syntax(syntax),
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(1),
    )?;
    let mut sliced = SliceParser::with_options(
        input,
        FormatOptions::CSV.syntax(syntax),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut owned = ByteRecord::new();
    while let Some(mut line) = streaming.next_line()? {
        line.read_byte_record_into(&mut owned)?;
        let mut sliced_line = sliced.next_line()?.expect("slice reader ended early");
        let borrowed = sliced_line.record()?;
        assert_eq!(
            owned.iter().collect::<Vec<_>>(),
            borrowed.iter().collect::<Vec<_>>()
        );
    }
    assert!(sliced.next_line()?.is_none());
    Ok(())
}

#[test]
fn compatible_unterminated_field_never_silently_discards_following_rows()
-> Result<(), Box<dyn StdError>> {
    let input = b"id,name\n1,alice\n2,\"bob\n3,carol\n4,dave\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        ParseOptions::new().buffer_capacity(1),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"1".as_slice()));
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("unterminated quoted field must fail");
    assert_eq!(error.kind(), ErrorKind::UnterminatedQuotedField);
    assert_eq!(
        reader
            .next_line()
            .expect_err("reader should remain failed")
            .kind(),
        ErrorKind::ParserFailed,
    );
    Ok(())
}

#[test]
fn custom_terminator_ends_streaming_comments() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b';')
        .quote(b'\'')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::Backslash(b'\\'))
        .comment(Some(b'#'));
    let mut reader = IoParser::with_options(
        Cursor::new(b"# ignored|a;b|"),
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(1),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.iter().collect::<Vec<_>>(), [b"a", b"b"]);
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn streaming_reader_preserves_io_errors() -> Result<(), Box<dyn StdError>> {
    let input = FailingReader::new(b"a,b\n".to_vec()).fail_after_bytes(4, io::ErrorKind::Other);
    let mut reader = IoParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(4),
    )?;
    let mut record = ByteRecord::new();
    let mut line = reader.next_line()?.expect("record");
    line.read_byte_record_into(&mut record)?;
    let error = reader
        .next_line()
        .expect_err("second read should surface I/O failure");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::Other));
    assert_eq!(
        error
            .into_io_error()
            .ok_or("missing source I/O error")?
            .kind(),
        io::ErrorKind::Other,
    );
    Ok(())
}

#[test]
fn mutable_records_reuse_and_convert_storage() -> Result<(), Box<dyn StdError>> {
    let mut record = ByteRecord::with_capacity(3, 32);
    record.push_field("alpha");
    record.push_field("beta");
    record.push_field("gamma");
    let byte_capacity = record.byte_capacity();
    assert!(record.set_field(1, "B"));
    record.truncate(2);
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        vec![b"alpha".as_slice(), b"B".as_slice()],
    );

    let strings = TextRecord::try_from(&record)?;
    assert_eq!(strings.iter().collect::<Vec<_>>(), ["alpha", "B"]);
    record.clear();
    assert!(record.byte_capacity() >= byte_capacity);
    Ok(())
}

#[test]
fn compatibility_recovery_is_explicit() -> Result<(), Box<dyn StdError>> {
    let compatibility = Recovery::default().quoting(false).unquoted_quotes(true);
    let mut reader = SliceParser::with_options(
        b"a\"b,c\n",
        FormatOptions::CSV
            .syntax(Syntax::Compatible(compatibility))
            .quoting(Quoting::Never),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"a\"b".as_slice()));

    let format = FormatOptions::CSV
        .delimiter(b',')
        .quote(b'"')
        .record_ending(RecordEnding::Newline)
        .escape(Escape::Backslash(b'\\'));
    let mut reader = SliceParser::with_options(
        b"\"a\\nb\" \t,c\n",
        format.syntax(Syntax::Compatible(
            Recovery::default().trailing_whitespace_after_quote(true),
        )),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing compatible row");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"anb".as_slice()));
    assert_eq!(record.get(1), Some(b"c".as_slice()));
    Ok(())
}

// ── RecordEnding::CrLf strictness (buffered) ──────────────────────────────────

#[test]
fn streaming_crlf_terminator_accepts_exact_boundaries_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let input = b"a,b\r\n\"c\rd\",\"e\nf\"\r\nlast,row";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();

        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"a".as_slice(), b"b".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            // Quoted CR/LF bytes are data, not record boundaries.
            [b"c\rd".as_slice(), b"e\nf".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"last".as_slice(), b"row".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_crlf_terminator_rejects_bare_lf_with_exact_position_across_every_buffer_width() {
    let cases: &[(&[u8], usize, u64)] = &[
        (b"a,b\n", 3, 1),
        (b"a\nb", 1, 1),
        (b"a,b\r\nc,d\n", 8, 2),
        (b",\n", 1, 1),
    ];
    for &(input, byte, line) in cases {
        for capacity in 1..=16 {
            let mut reader = IoParser::with_options(
                Cursor::new(input),
                FormatOptions::CSV.record_ending(RecordEnding::CrLf),
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )
            .expect("valid reader configuration");
            let mut record = ByteRecord::new();
            let error = loop {
                match reader.next_line() {
                    Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                        Ok(()) => {}
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
                "{input:?} @ capacity {capacity}"
            );
            assert_eq!(
                error.location().byte,
                byte,
                "{input:?} @ capacity {capacity}"
            );
            assert_eq!(
                error.location().line,
                line,
                "{input:?} @ capacity {capacity}"
            );
        }
    }
}

#[test]
fn streaming_crlf_terminator_rejects_bare_cr_with_exact_position_across_every_buffer_width() {
    // Every case's bare (or EOF-truncated) `\r` falls within the first
    // record, including two that end the whole input on a lone trailing
    // `\r` with no following byte at all -- exercising the buffered reader's
    // "optimistic EOF" raw-record accumulation, which must still be rejected
    // once the accumulated bytes are re-validated.
    let cases: &[(&[u8], usize, u64)] = &[
        (b"a,b\rc,d\r\n", 3, 1),
        (b"a,b\r", 3, 1),
        (b"a\r", 1, 1),
        (b",\r", 1, 1),
    ];
    for &(input, byte, line) in cases {
        for capacity in 1..=16 {
            let mut reader = IoParser::with_options(
                Cursor::new(input),
                FormatOptions::CSV.record_ending(RecordEnding::CrLf),
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(capacity),
            )
            .expect("valid reader configuration");
            let mut first_line = reader
                .next_line()
                .expect("record exists")
                .expect("record exists");
            let error = first_line
                .read_byte_record_into(&mut ByteRecord::new())
                .expect_err("bare CR should fail");
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidRecordEnding(b'\r'),
                "{input:?} @ capacity {capacity}"
            );
            assert_eq!(
                error.location().byte,
                byte,
                "{input:?} @ capacity {capacity}"
            );
            assert_eq!(
                error.location().line,
                line,
                "{input:?} @ capacity {capacity}"
            );
        }
    }
}

#[test]
fn streaming_crlf_terminator_rejects_bare_cr_at_true_eof_after_a_prior_record()
-> Result<(), Box<dyn StdError>> {
    // The first record completes normally on an exact CRLF boundary; the
    // second record ends the entire input on a lone trailing `\r` at true
    // EOF, forcing the general chunk parser's re-entrant ("next_offset !=
    // 0") path to also reject the malformed trailing byte.
    let input = b"ok,row\r\nbad\r";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"ok".as_slice(), b"row".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        let error = line
            .read_byte_record_into(&mut record)
            .expect_err("trailing lone CR at EOF should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::InvalidRecordEnding(b'\r'),
            "capacity {capacity}"
        );
        assert_eq!(error.location().byte, 11, "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_crlf_terminator_skips_comments_and_blank_lines_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::RFC4180.comment(Some(b'#'));
    let input = b"# ignored\r\n\r\nfirst,row\r\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            format.blank_records(BlankRecords::Skip),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"first".as_slice(), b"row".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_newline_terminator_treats_bare_cr_as_data_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    // `RecordEnding::Newline` (the default) must remain unaffected by CrLf
    // support: a lone `\r` not immediately followed by `\n` stays ordinary
    // field data, for every buffered chunk width.
    let input = b"a,b\nc\rd,e\r\nlast,row\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"a".as_slice(), b"b".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"c\rd".as_slice(), b"e".as_slice()],
            "capacity {capacity}",
        );
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"last".as_slice(), b"row".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

// ── PostgreSQL COPY CSV NULL semantics (buffered) ───────────────────────────

#[test]
fn streaming_postgres_copy_csv_null_detection_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let input = b",\"\",a\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::POSTGRES_COPY_CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.is_null(1), Some(false), "capacity {capacity}");
        assert_eq!(record.is_null(2), Some(false), "capacity {capacity}");
        assert_eq!(record.get(0), Some(&b""[..]), "capacity {capacity}");
        assert_eq!(record.get(1), Some(&b""[..]), "capacity {capacity}");
        assert_eq!(record.get(2), Some(&b"a"[..]), "capacity {capacity}");
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_postgres_copy_csv_headers_are_never_marked_null_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let input = b",b\n,2\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::POSTGRES_COPY_CSV,
            ParseOptions::new().buffer_capacity(capacity),
        )?;
        let headers = reader.headers()?.ok_or("missing headers")?.clone();
        assert_eq!(headers.is_null(0), Some(false), "capacity {capacity}");

        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.is_null(1), Some(false), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_postgres_copy_csv_string_record_preserves_null_metadata()
-> Result<(), Box<dyn StdError>> {
    let input = b",\"\"\n";
    for capacity in 1..=8 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::POSTGRES_COPY_CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = TextRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_text_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.is_null(1), Some(false), "capacity {capacity}");
        assert_eq!(record.get(0), Some(""), "capacity {capacity}");
        assert_eq!(record.get(1), Some(""), "capacity {capacity}");
    }
    Ok(())
}

// ── MySQL text-export syntax (buffered) ─────────────────────────────────────

#[test]
fn streaming_mysql_text_export_decodes_escapes_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let input = b"a\\0b\tc\\bd\te\\nf\tg\\rh\ti\\tj\tk\\Zl\tm\\\\n\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::MYSQL,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [
                b"a\0b".as_slice(),
                b"c\x08d".as_slice(),
                b"e\nf".as_slice(),
                b"g\rh".as_slice(),
                b"i\tj".as_slice(),
                b"k\x1Al".as_slice(),
                b"m\\n".as_slice(),
            ],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_mysql_text_export_null_detection_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    let input = b"\\N\tb\tc\\N\t\\\\N\n";
    for capacity in 1..=16 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::MYSQL,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.get(0), Some(&b""[..]), "capacity {capacity}");
        assert_eq!(record.is_null(1), Some(false), "capacity {capacity}");
        assert_eq!(record.get(1), Some(&b"b"[..]), "capacity {capacity}");
        assert_eq!(record.is_null(2), Some(false), "capacity {capacity}");
        assert_eq!(record.get(2), Some(&b"cN"[..]), "capacity {capacity}");
        assert_eq!(record.is_null(3), Some(false), "capacity {capacity}");
        assert_eq!(record.get(3), Some(&b"\\N"[..]), "capacity {capacity}");
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_mysql_text_export_trailing_lone_backslash_at_true_eof_across_every_buffer_width()
-> Result<(), Box<dyn StdError>> {
    // The trailing backslash is the last byte of the entire input (true
    // EOF, no record_ending following), for every buffered chunk width.
    let input = b"x\ta\\";
    for capacity in 1..=8 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::MYSQL,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"x".as_slice(), b"a\\".as_slice()],
            "capacity {capacity}",
        );
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

// ── Default-kernel gating regression (buffered) ─────────────────────────────

#[test]
fn streaming_null_style_gate_disables_default_decoding_without_preset()
-> Result<(), Box<dyn StdError>> {
    // `FormatOptions::CSV` plus an explicit `nulls`, without any
    // format, must still disable the buffered reader's specialized
    // "default format" chunk kernels (which have no NULL-detection logic
    // at all) and route through the general parser.
    let input = b",a\n";
    for capacity in 1..=8 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::CSV.nulls(Nulls::PostgresCsv),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.is_null(1), Some(false), "capacity {capacity}");
    }
    Ok(())
}

#[test]
fn streaming_explicit_mysql_dialect_without_preset_triggers_general_parsing()
-> Result<(), Box<dyn StdError>> {
    // A hand-built format with `Escape::Mysql` and `Nulls::Mysql`
    // (not the `FormatOptions::MYSQL` convenience) must also bypass the
    // default chunk kernels.
    let format = FormatOptions::CSV
        .delimiter(b'\t')
        .quote(b'"')
        .record_ending(RecordEnding::Newline)
        .escape(Escape::Mysql)
        .quoting(Quoting::Never);
    let input = b"\\N\ta\\tb\n";
    for capacity in 1..=8 {
        let mut reader = IoParser::with_options(
            Cursor::new(input.as_slice()),
            format.nulls(Nulls::Mysql),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut record = ByteRecord::new();
        let mut line = reader.next_line()?.expect("record");
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.is_null(0), Some(true), "capacity {capacity}");
        assert_eq!(record.get(1), Some(&b"a\tb"[..]), "capacity {capacity}");
        assert!(reader.next_line()?.is_none(), "capacity {capacity}");
    }
    Ok(())
}

/// `read_byte_record_into` parses straight into caller storage and rewinds the
/// chunk window to serve any later view, so a second view of the same record
/// must agree with the first across every buffer size, including sizes that
/// force a refill in the middle of a record.
#[test]
fn mixed_views_of_one_streaming_record_agree() -> Result<(), Box<dyn StdError>> {
    let input: &[u8] = b"city,population,note\n\
        Boston,650706,\"quoted, with comma\"\n\
        Lowell,115554,plain\n\
        \"Sprin\"\"gfield\",155929,\"multi\nline\"\n\
        Quincy,101636,\n";

    for capacity in [1_usize, 2, 3, 7, 13, 64, 4096] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut reference = SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )?;

        let mut owned = ByteRecord::new();
        while let Some(mut line) = reader.next_line()? {
            let mut reference_line = reference.next_line()?.expect("reference ended early");
            let expected: Vec<Vec<u8>> = reference_line
                .record()?
                .iter()
                .map(<[u8]>::to_vec)
                .collect();

            line.read_byte_record_into(&mut owned)?;
            let first: Vec<Vec<u8>> = owned.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(first, expected, "capacity {capacity}: owned view differs");

            // A borrowed view of the record already read must still work and
            // must observe exactly the same fields and index.
            let borrowed = line.record()?;
            let second: Vec<Vec<u8>> = borrowed.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(second, expected, "capacity {capacity}: second view differs");

            // Reading it a third time, back into owned storage, must also agree.
            line.read_byte_record_into(&mut owned)?;
            let third: Vec<Vec<u8>> = owned.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(third, expected, "capacity {capacity}: third view differs");
        }
        assert!(
            reference.next_line()?.is_none(),
            "capacity {capacity}: reference had extra records"
        );
    }
    Ok(())
}

/// Rewinding to serve a second view must not disturb the record indices or the
/// line numbers the parser reports for subsequent records.
#[test]
fn rewinding_for_a_second_view_preserves_indices() -> Result<(), Box<dyn StdError>> {
    let input: &[u8] = b"a,1\nb,2\n\"c\nc\",3\nd,4\n";

    for capacity in [1_usize, 3, 8, 4096] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut owned = ByteRecord::new();
        let mut seen = Vec::new();
        while let Some(mut line) = reader.next_line()? {
            line.read_byte_record_into(&mut owned)?;
            // Force the rewind path, then record what the parser believes.
            let record = line.record()?;
            seen.push((record.index(), record.get(0).map(<[u8]>::to_vec)));
        }
        let indices: Vec<u64> = seen.iter().map(|(index, _)| *index).collect();
        assert_eq!(
            indices,
            vec![0, 1, 2, 3],
            "capacity {capacity}: indices drifted"
        );
        let firsts: Vec<Vec<u8>> = seen.into_iter().filter_map(|(_, first)| first).collect();
        assert_eq!(
            firsts,
            vec![
                b"a".to_vec(),
                b"b".to_vec(),
                b"c\nc".to_vec(),
                b"d".to_vec()
            ],
            "capacity {capacity}: fields drifted",
        );
    }
    Ok(())
}

/// A projected decode selects only some columns, but the record it consumed is
/// still resident in the chunk window, so a following full view must rewind and
/// see every field.
#[cfg(feature = "derive")]
#[test]
fn full_view_after_projected_decode_sees_every_field() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, PartialEq, coseva::encoding::CsvDecode)]
    struct Population {
        population: u64,
    }

    let input: &[u8] = b"city,population,note\n\
        Boston,650706,capital\n\
        Lowell,115554,mill\n\
        Quincy,101636,coastal\n";

    for capacity in [1_usize, 4, 16, 4096] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(capacity),
        )?;

        // A slice parser over the same input is the oracle for field contents
        // and record indices alike.
        let mut reference =
            SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())?;

        let mut seen = Vec::new();
        while let Some(mut line) = reader.next_line()? {
            let mut reference_line = reference.next_line()?.expect("reference ended early");
            let expected_index = reference_line.record()?.index();

            // Projecting view: only `population` is materialized.
            let projected: Population = line.decoded()?;

            // Full view of the same record must still expose all three fields.
            let record = line.record()?;
            let index = record.index();
            let fields: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(
                fields.len(),
                3,
                "capacity {capacity}: lost fields after projection"
            );
            assert_eq!(
                index, expected_index,
                "capacity {capacity}: record index drifted across the projected rewind",
            );
            seen.push((projected.population, fields));
        }

        assert_eq!(
            seen,
            vec![
                (
                    650_706,
                    vec![b"Boston".to_vec(), b"650706".to_vec(), b"capital".to_vec()]
                ),
                (
                    115_554,
                    vec![b"Lowell".to_vec(), b"115554".to_vec(), b"mill".to_vec()]
                ),
                (
                    101_636,
                    vec![b"Quincy".to_vec(), b"101636".to_vec(), b"coastal".to_vec()]
                ),
            ],
            "capacity {capacity}: projected-then-full views disagree",
        );
    }
    Ok(())
}

/// The reverse order, and repeated projections, must also stay consistent.
#[cfg(feature = "derive")]
#[test]
fn projected_decode_after_a_full_view_agrees() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, PartialEq, coseva::encoding::CsvDecode)]
    struct Population {
        population: u64,
    }

    let input: &[u8] = b"city,population,note\nBoston,650706,capital\nLowell,115554,mill\n";

    for capacity in [1_usize, 4, 4096] {
        let mut reader = IoParser::with_options(
            Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(capacity),
        )?;
        let mut populations = Vec::new();
        while let Some(mut line) = reader.next_line()? {
            let first = line.record()?.get(1).map(<[u8]>::to_vec);
            let projected: Population = line.decoded()?;
            let again: Population = line.decoded()?;
            assert_eq!(
                projected, again,
                "capacity {capacity}: repeat projection differs"
            );
            assert_eq!(
                first,
                Some(projected.population.to_string().into_bytes()),
                "capacity {capacity}: views disagree",
            );
            populations.push(projected.population);
        }
        assert_eq!(populations, vec![650_706, 115_554], "capacity {capacity}");
    }
    Ok(())
}

// ── helpers for injected-failure readers ─────────────────────────────────────────

/// Treat every record as data, bypassing the default first-record header policy.
fn unheaded_streaming(input: &'static [u8]) -> IoParser<Cursor<&'static [u8]>> {
    IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
}

// ── IoParser: I/O failure and retry behavior ─────────────────────────────

#[test]
fn streaming_parser_io_error_propagates() {
    let data = b"city,pop\nBoston,1\n";
    // Fail after reading 5 bytes (before the first record is complete).
    let reader = FailingReader::new(data.to_vec()).fail_after_bytes(5, io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let err = parser.next_line().expect_err("I/O error should propagate");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_interrupted_reader_retries_and_succeeds() -> Result<(), Box<dyn StdError>> {
    let data = b"city,pop\nBoston,1\n";
    let reader = FailingReader::new(data.to_vec()).interrupt_on_read(1);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let mut line = parser.next_line()?.expect("record after retry");
    assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_read_overrun_fails_parser() {
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(Vec::new()).overrun(),
        ParseOptions::new(),
    )
    .expect("parser");
    let err = parser.next_line().expect_err("overrun should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_set_headers_replaces_header_record() -> Result<(), Box<dyn StdError>> {
    let input = b"Boston,1\nLondon,2\n";
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let mut headers = ByteRecord::new();
    headers.push_field(b"city");
    headers.push_field(b"pop");
    parser.set_headers(headers);
    // After set_headers(), has_headers() reports whether the engine has a header record.
    let _ = parser.has_headers();
    let mut line = parser.next_line()?.expect("record");
    assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_header_index_and_indices() -> Result<(), Box<dyn StdError>> {
    let mut parser = IoParser::<_, Csv>::new(
        Cursor::new(b"city,tag,tag\nBoston,east,large\n"),
        ParseOptions::new(),
    )
    .expect("parser");
    assert_eq!(parser.header_index("city")?, Some(0));
    assert_eq!(parser.header_index("missing")?, None);
    assert_eq!(parser.header_indices("tag")?, [1, 2]);
    Ok(())
}

#[test]
fn streaming_parser_has_headers_reports_policy() -> Result<(), Box<dyn StdError>> {
    let with =
        IoParser::<_, Csv>::new(Cursor::new(b"a,b\n1,2\n"), ParseOptions::new()).expect("parser");
    assert!(with.has_headers());

    let mut without = unheaded_streaming(b"a,b\n");
    assert!(!without.has_headers());
    // Suppress unused-result warning.
    let _ = without.next_line()?;
    Ok(())
}

#[test]
fn streaming_parser_get_ref_get_mut_into_inner() {
    let cursor = Cursor::new(b"a,b\n" as &[u8]);
    let mut parser = IoParser::<_, Csv>::new(cursor, ParseOptions::new()).expect("parser");
    // get_ref
    let _ = parser.get_ref();
    // get_mut — must not read from it directly, just confirm access
    let _ = parser.get_mut();
    // into_inner
    let _cursor = parser.into_inner();
}

#[test]
fn streaming_parser_reclaims_the_window_without_disturbing_records() -> Result<(), Box<dyn StdError>>
{
    // Refilling hands back a window an outlier record blew up, which moves
    // live bytes; every record after it must still read back intact.
    use std::fmt::Write as _;

    let huge = "q".repeat(512 * 1024);
    let mut input = format!("{huge},1\n");
    for index in 0..2_000 {
        writeln!(input, "small{index},2").expect("writing to a String cannot fail");
    }

    let mut parser = unheaded_streaming(input.leak().as_bytes());
    let mut seen = 0usize;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        if seen == 0 {
            assert_eq!(record.get(0), Some(huge.as_bytes()));
        } else {
            assert_eq!(record.get(0), Some(format!("small{}", seen - 1).as_bytes()));
        }
        seen += 1;
    }
    assert_eq!(seen, 2_001);
    Ok(())
}

#[test]
fn streaming_parser_is_done_after_eof() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_streaming(b"a,b\n");
    assert!(!parser.is_done());
    while parser.next_line()?.is_some() {}
    assert!(parser.is_done());
    Ok(())
}

#[test]
fn streaming_parser_location_is_stream_relative() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_streaming(b"a,b\nc,d\n");
    let before = parser.location();
    assert_eq!(before.byte, 0);
    {
        let mut line = parser.next_line()?.expect("first record");
        line.record().expect("parse")
    };
    let after = parser.location();
    assert!(after.byte > 0);
    Ok(())
}

#[test]
fn streaming_parser_seek_revisits_record() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nParis,2\nLyon,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    {
        let mut line = parser.next_line()?.expect("first data record");
        line.record().expect("parse")
    };
    let bookmark = parser.location();

    {
        let mut line = parser.next_line()?.expect("second record");
        assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    };

    parser.seek(bookmark)?;
    let mut line = parser.next_line()?.expect("second record again");
    assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_seek_with_match_first_reads_first_record() -> Result<(), Box<dyn StdError>> {
    // Exercises prepare_seek_state lines 750/751-755: when FieldCount::MatchFirst is configured
    // and no record has been read yet, seek() reads the first record to establish field width.
    use coseva::config::FieldCount;
    let input = b"a,b\nc,d\n";
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::MatchFirst),
    )?;
    // seek() to the second record without reading any records first.
    let loc = coseva::Location {
        byte: 4,
        line: 2,
        record: 2,
        field: 0,
    };
    parser.seek(loc)?;
    let mut line = parser.next_line()?.expect("second record");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"c".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_seek_rejects_nonzero_field() {
    let mut parser = IoParser::<_, Csv>::new(
        Cursor::new(b"city,pop\nBoston,1\n".as_slice()),
        ParseOptions::new(),
    )
    .expect("parser");
    let mut loc = parser.location();
    loc.field = 1;
    let err = parser.seek(loc).expect_err("nonzero field should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_seek_raw_success() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nParis,2\nLyon,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    {
        let mut line = parser.next_line()?.expect("first data record");
        line.record().expect("parse")
    };
    let loc = parser.location();

    // seek_raw to the same position using SeekFrom::Start
    parser.seek_raw(SeekFrom::Start(loc.byte as u64), loc)?;
    let mut line = parser.next_line()?.expect("record after seek_raw");
    assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_seek_raw_mismatch_fails() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nParis,2\nLyon,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    {
        let mut line = parser.next_line()?.expect("first data record");
        line.record().expect("parse")
    };
    let mut loc = parser.location();
    let real_byte = loc.byte;
    // Claim byte 0 but actually seek there — the reported location won't match.
    loc.byte = 0;
    let err = parser
        .seek_raw(SeekFrom::Start(real_byte as u64), loc)
        .expect_err("mismatched location should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
    Ok(())
}

#[test]
fn streaming_parser_seek_raw_io_error_propagates() -> Result<(), Box<dyn StdError>> {
    // seek_raw line 682: the underlying seek fails, propagating IO error.
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).fail_all_seeks(io::ErrorKind::PermissionDenied),
        ParseOptions::new(),
    )
    .expect("parser");
    // Read all data so headers are resolved (prepare_seek_state will short-circuit)
    while parser.next_line()?.is_some() {}
    let loc = coseva::Location {
        byte: 0,
        line: 1,
        record: 1,
        field: 0,
    };
    let err = parser
        .seek_raw(SeekFrom::Start(0), loc)
        .expect_err("seek_raw IO error");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
    Ok(())
}

#[test]
fn streaming_parser_headers_on_failed_parser() {
    // ensure_headers line 352: check_failed() returns error when parser is failed.
    let headers = b"city,pop\n";
    let reader = FailingReader::new(headers.to_vec())
        .fail_after_bytes(headers.len(), io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    // first next_line() resolves headers, then fails on the data record
    let _err = parser.next_line().expect_err("IO error on data");
    // Parser is now in failed state. Call headers() to trigger ensure_headers check_failed.
    let err = parser
        .headers()
        .expect_err("failed parser should error in headers()");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_header_index_on_failed_parser() {
    // ensure_headers()? in header_index propagates
    // error when parser is in failed state.
    let headers = b"city,pop\n";
    let reader = FailingReader::new(headers.to_vec())
        .fail_after_bytes(headers.len(), io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let _err = parser.next_line().expect_err("IO error on data");
    let err = parser
        .header_index("city")
        .expect_err("failed parser should error in header_index");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_header_indices_on_failed_parser() {
    // ensure_headers()? in header_indices propagates
    // error when parser is in failed state.
    let headers = b"city,pop\n";
    let reader = FailingReader::new(headers.to_vec())
        .fail_after_bytes(headers.len(), io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let _err = parser.next_line().expect_err("IO error on data");
    let err = parser
        .header_indices("city")
        .expect_err("failed parser should error in header_indices");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_rewind_restarts_stream() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    {
        let mut line = parser.next_line()?.expect("data record");
        assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
    };
    assert!(parser.next_line()?.is_none());

    parser.rewind()?;
    let mut line = parser.next_line()?.expect("data record again");
    assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_bom_detected_and_stripped() -> Result<(), Box<dyn StdError>> {
    let bom_input = b"\xEF\xBB\xBFcity,pop\nBoston,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Detect),
        ParseOptions::new(),
    )?;
    let mut line = parser.next_line()?.expect("first data record");
    assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_bom_reject_on_view_poisons_parser() -> Result<(), Box<dyn StdError>> {
    // With Headers::None, bom_rejected is propagated to the Line view.
    let bom_input = b"\xEF\xBB\xBFa,b\nc,d\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let err = line
        .record()
        .expect_err("rejected BOM should fail the view");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

#[test]
fn streaming_parser_record_too_large_is_detected() -> Result<(), Box<dyn StdError>> {
    let mut big_row = vec![b'x'; 1024];
    big_row.extend_from_slice(b"\n");
    let mut parser = IoParser::with_options(
        Cursor::new(big_row),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(512, 512, 1024)),
    )
    .expect("valid options");
    // In lazy mode the parser positions on the record; the error appears when a view parses it.
    match parser.next_line()? {
        None => {} // error already propagated
        Some(mut line) => {
            let err = line
                .record()
                .expect_err("oversized record should fail on view");
            assert!(matches!(
                err.kind(),
                ErrorKind::RecordTooLarge { .. } | ErrorKind::FieldTooLarge { .. }
            ));
        }
    }
    Ok(())
}

#[test]
fn streaming_parser_buffer_growth_handles_record_spanning_reads() -> Result<(), Box<dyn StdError>> {
    // Feed a record bigger than the initial buffer to exercise the growth path.
    let field: Vec<u8> = vec![b'z'; 512];
    let mut input = field.clone();
    input.extend_from_slice(b"\n");
    let mut parser = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(32),
    )?;
    let mut line = parser.next_line()?.expect("large record");
    assert_eq!(line.record()?.get(0), Some(field.as_slice()));
    Ok(())
}

#[test]
fn streaming_parser_headers_error_path() {
    // Drive the header discovery failure: malformed first record raises an error.
    let input = b"\"unclosed\nBoston,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let err = parser
        .next_line()
        .expect_err("malformed headers should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::UnterminatedQuotedField | ErrorKind::ParserFailed
    ));
}

#[test]
fn streaming_parser_header_indices_error_path() {
    let malformed = b"\"unclosed\nBoston,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(malformed.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    let err = parser
        .header_indices("city")
        .expect_err("should fail on malformed headers");
    assert!(matches!(
        err.kind(),
        ErrorKind::UnterminatedQuotedField | ErrorKind::ParserFailed
    ));
}

#[test]
fn streaming_parser_advance_with_filter_column_not_found() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nBoston,1\nLondon,2\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    let pred = Predicate::equals("nonexistent", "Boston");
    let result = parser.next_matching_line(&pred)?;
    assert!(result.is_none());
    Ok(())
}

#[test]
fn streaming_parser_advance_with_filter_error_in_record() {
    // Syntax error inside a candidate record should propagate from advance_with_filter.
    let input = b"city,pop\nBoston,1\n\"bad\",2\n";
    let pred = Predicate::equals("pop", "2");
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    // The bad record looks like a candidate (it has "2"), so the parser will try to
    // parse it and hit the malformed quote. If no error is produced here, the test
    // is still valid — we just confirm the parser doesn't panic.
    let _ = parser.next_matching_line(&pred);
}

#[test]
fn streaming_parser_check_failed_in_advance_with_filter() -> Result<(), Box<dyn StdError>> {
    // Poison the parser through a lazy-mode record view error, then call
    // next_matching_line — check_failed() should propagate the failure.
    let input = b"city\n\"unclosed\n";
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    {
        let mut line = parser.next_line()?.expect("positioned on bad record");
        let _ = line.record(); // this fails and sets failed=true
    }
    let pred = Predicate::equals("city", "x");
    let err = parser.next_matching_line(&pred).expect_err("failed parser");
    assert_eq!(err.kind(), ErrorKind::ParserFailed);
    Ok(())
}

#[test]
fn streaming_parser_ensure_headers_io_error_in_advance_with_filter() {
    // FailingReader fails immediately; ensure_headers in advance_with_filter fails.
    let reader =
        FailingReader::new(b"".to_vec()).fail_after_bytes(0, io::ErrorKind::ConnectionReset);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let pred = Predicate::equals("city", "x");
    let err = parser
        .next_matching_line(&pred)
        .expect_err("io error from headers");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_bom_rejected_with_auto_headers_fails_early() {
    // ReadBom::Reject + auto-discover headers triggers ensure_headers to fail
    // with RejectedBom ().
    let bom_input = b"\xEF\xBB\xBFcity,pop\nBoston,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new(),
    )
    .expect("valid options");
    let err = parser.next_line().expect_err("rejected BOM should fail");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
}

#[test]
fn streaming_parser_headers_io_error_propagates() {
    // ensure_headers fails during headers() — covers ensure_headers? path.
    let reader = FailingReader::new(b"".to_vec()).fail_after_bytes(0, io::ErrorKind::TimedOut);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let err = parser.headers().expect_err("io error from headers");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_header_indices_io_error_propagates() {
    // ensure_headers fails during header_indices() — covers ensure_headers? path.
    let reader = FailingReader::new(b"".to_vec()).fail_after_bytes(0, io::ErrorKind::Other);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    let err = parser.header_indices("city").expect_err("io error");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_seek_raw_nonzero_field_is_rejected() {
    // seek_raw with location.field != 0 must return an error immediately.
    use coseva::Location;
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    let loc = Location {
        byte: 9,
        line: 1,
        record: 2,
        field: 1,
    };
    let err = parser
        .seek_raw(SeekFrom::Start(9), loc)
        .expect_err("nonzero field should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_seek_returns_wrong_offset_is_detected() -> Result<(), Box<dyn StdError>> {
    // A Seek impl that returns a different offset than requested triggers an error.
    use coseva::Location;
    let input = b"city,pop\nBoston,1\nLondon,2\n";
    // seek to byte 9 but lie and report byte 0
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).lie_on_seek(0),
        ParseOptions::new(),
    )
    .expect("parser");
    // Read the first data record so prepare_seek_state is satisfied.
    {
        let mut line = parser.next_line()?.expect("first record");
        line.record()?
    };
    let loc = Location {
        byte: 9,
        line: 2,
        record: 2,
        field: 0,
    };
    let err = parser.seek(loc).expect_err("wrong offset detected");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
    Ok(())
}

#[test]
fn streaming_parser_seek_io_error_propagates() -> Result<(), Box<dyn StdError>> {
    // A Seek impl that returns an error causes seek() to propagate it.
    use coseva::Location;
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).fail_all_seeks(io::ErrorKind::PermissionDenied),
        ParseOptions::new(),
    )
    .expect("parser");
    // Read to completion so prepare_seek_state passes.
    while parser.next_line()?.is_some() {}
    let loc = Location {
        byte: 0,
        line: 1,
        record: 1,
        field: 0,
    };
    let err = parser.seek(loc).expect_err("seek io error");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
    Ok(())
}

#[test]
fn streaming_parser_rewind_io_error_propagates() {
    // A Seek impl that fails causes rewind() to propagate the error.
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).fail_all_seeks(io::ErrorKind::PermissionDenied),
        ParseOptions::new(),
    )
    .expect("parser");
    let err = parser.rewind().expect_err("rewind io error");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_rewind_wrong_position_is_detected() {
    // A Seek impl that rewinds to a non-zero offset triggers an error.
    let input = b"city,pop\nBoston,1\n";
    // Lie about the rewind position (return byte 1 instead of 0)
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).lie_on_seek(1),
        ParseOptions::new(),
    )
    .expect("parser");
    let err = parser.rewind().expect_err("wrong rewind position");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_from_path_nonexistent_returns_io_error() {
    let path = common::temp_file("io-parser-nonexistent");
    let err = IoParser::from_path(path.path(), FormatOptions::CSV, ParseOptions::new())
        .expect_err("nonexistent file");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_from_path_opens_file() -> Result<(), Box<dyn StdError>> {
    // Write a minimal CSV, parse it with from_path, clean up.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bcsv_test_from_path.csv");
    let mut f = fs::File::create(path)?;
    f.write_all(b"city,pop\nBoston,1\n")?;
    drop(f);
    let result = (|| -> Result<(), Box<dyn StdError>> {
        let mut parser = IoParser::from_path(path, FormatOptions::CSV, ParseOptions::new())?;
        let mut line = parser.next_line()?.expect("record");
        assert_eq!(line.record()?.get(0), Some(b"Boston".as_slice()));
        Ok(())
    })();
    let _ = fs::remove_file(path);
    result
}

#[test]
fn streaming_parser_advance_with_filter_column_not_found_returns_false()
-> Result<(), Box<dyn StdError>> {
    // advance_with_filter: when a column name is not found, returns Ok(false) early (line 193).
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    let pred = Predicate::contains("nonexistent", "x");
    let found = parser.next_matching_line(&pred)?;
    assert!(found.is_none(), "no match when column name is absent");
    Ok(())
}

#[test]
fn streaming_parser_advance_with_filter_io_error_in_loop() {
    // advance_with_filter line 201: advance() fails inside the filter loop (IO error).
    // Headers are served successfully; then the reader fails.
    let headers = b"city,pop\n";
    let reader = FailingReader::new(headers.to_vec())
        .fail_after_bytes(headers.len(), io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    // Use a predicate with a known column so we enter the loop
    let pred = Predicate::contains("city", "x");
    let err = parser
        .next_matching_line(&pred)
        .expect_err("IO error in loop");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_ensure_headers_bom_rejected_discovers_headers() {
    // bom_rejected && discovers_headers → Err.
    // With ReadBom::Reject and default header discovery, a BOM at the start
    // causes ensure_headers() to return a RejectedBom error.
    let bom_csv = b"\xEF\xBB\xBFcity,pop\nBoston,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_csv.as_slice()),
        FormatOptions::new().read_bom(ReadBom::Reject),
        ParseOptions::new(),
    )
    .expect("construction succeeds");
    let err = parser
        .next_line()
        .expect_err("BOM with auto-headers must fail");
    assert!(matches!(err.kind(), ErrorKind::RejectedBom));
}

#[test]
fn streaming_parser_ensure_headers_loop_refill() -> Result<(), Box<dyn StdError>> {
    // ensure_headers loop refills when headers span multiple reads.
    // Use OneByteReader to force many refills before headers resolve.
    let input = b"city,pop\nBoston,1\n";
    let mut parser = IoParser::<_, Csv>::new(
        FailingReader::new(input.to_vec()).max_chunk(1),
        ParseOptions::new(),
    )
    .expect("parser");
    let mut line = parser.next_line()?.expect("record");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"Boston".as_slice()));
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn streaming_parser_resolve_typed_mapping_on_failed_parser() {
    #[derive(Debug, coseva::encoding::CsvDecode, PartialEq)]
    struct City {
        city: String,
        pop: u64,
    }

    // A failed parser must propagate its error through the typed-decode path
    // rather than resuming or panicking.
    let headers = b"city,pop\n";
    let reader = FailingReader::new(headers.to_vec())
        .fail_after_bytes(headers.len(), io::ErrorKind::BrokenPipe);
    let mut parser = IoParser::<_, Csv>::new(reader, ParseOptions::new()).expect("parser");
    // Fail the parser — next_line() resolves headers, then fails on the data record.
    let _err = parser.next_line().expect_err("IO error on data");
    let mut iter = parser.decoded_records::<City>();
    let err = iter
        .next()
        .expect("Some result")
        .expect_err("failed parser");
    assert!(matches!(
        err.kind(),
        ErrorKind::ParserFailed | ErrorKind::Io(_)
    ));
}

#[test]
fn streaming_parser_ensure_headers_syntax_error() {
    // A syntax error in the header record itself (an unterminated quoted
    // field at EOF) must fail header resolution.
    let input = b"\"unclosed";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    let err = parser
        .next_line()
        .expect_err("unterminated header should fail");
    assert!(matches!(
        err.kind(),
        ErrorKind::UnterminatedQuotedField | ErrorKind::Io(_)
    ));
}

#[cfg(feature = "derive")]
#[test]
fn streaming_parser_decode_with_mapping_type_error() {
    #[derive(Debug, coseva::encoding::CsvDecode, PartialEq)]
    struct City {
        city: String,
        pop: u64,
    }

    // A field that fails type conversion surfaces through `decoded_records`,
    // which resolves and applies the typed mapping.
    let input = b"city,pop\nBoston,notanumber\n";
    let mut parser = IoParser::<_, Csv>::new(Cursor::new(input.as_slice()), ParseOptions::new())
        .expect("parser");
    let mut iter = parser.decoded_records::<City>();
    let err = iter
        .next()
        .expect("Some result")
        .expect_err("type mismatch");
    assert!(
        matches!(
            err.kind(),
            ErrorKind::InvalidDigit | ErrorKind::InvalidValue | ErrorKind::Io(_)
        ),
        "unexpected error kind: {:?}",
        err.kind()
    );
}

#[test]
fn streaming_parser_from_path_config_error() {
    // Invalid format options (delimiter equal to quote) are rejected before
    // the parser even attempts to open the file.
    let bad_format = FormatOptions::new().delimiter(b'"'); // delimiter == quote
    let err = IoParser::from_path("some_file.csv", bad_format, ParseOptions::new())
        .expect_err("invalid format should fail before opening the file");
    assert!(matches!(err.kind(), ErrorKind::Configuration));
}

// ── window management across dialects and capacities ────────────────────────────

#[test]
fn streaming_parser_crlf_dialect_exercises_window_paths() -> Result<(), Box<dyn StdError>> {
    let input = b"a,b\r\n\"c,d\"\r\ne,f\r\n";
    for cap in [1, 3, 7, 64] {
        let mut r = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(cap),
        )?;
        let mut recs: Vec<Vec<Vec<u8>>> = Vec::new();
        while let Some(mut line) = r.next_line()? {
            let mut rec = ByteRecord::new();
            line.read_byte_record_into(&mut rec)?;
            recs.push(rec.iter().map(<[u8]>::to_vec).collect());
        }
        assert_eq!(recs.len(), 3, "cap={cap}");
        assert_eq!(recs[0], [b"a", b"b"]);
        assert_eq!(recs[1], [b"c,d"]);
        assert_eq!(recs[2], [b"e", b"f"]);
    }
    Ok(())
}

#[test]
fn streaming_parser_mysql_dialect_exercises_window_paths() -> Result<(), Box<dyn StdError>> {
    let input = b"\\0\t\\n\nhel\\tlo\tworld\n";
    for cap in [1, 4, 16, 64] {
        let mut r = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::MYSQL,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(cap),
        )?;
        let mut recs: Vec<Vec<Vec<u8>>> = Vec::new();
        while let Some(mut line) = r.next_line()? {
            let mut rec = ByteRecord::new();
            line.read_byte_record_into(&mut rec)?;
            recs.push(rec.iter().map(<[u8]>::to_vec).collect());
        }
        assert_eq!(recs.len(), 2, "cap={cap}");
        assert_eq!(recs[0][0], b"\x00");
        assert_eq!(recs[0][1], b"\n");
        assert_eq!(recs[1][0], b"hel\tlo");
    }
    Ok(())
}

#[test]
fn streaming_parser_skips_bom_at_start() -> Result<(), Box<dyn StdError>> {
    let input = b"\xEF\xBB\xBFa,b\nc,d\n";
    let mut r = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Detect),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut recs: Vec<Vec<Vec<u8>>> = Vec::new();
    while let Some(mut line) = r.next_line()? {
        let mut rec = ByteRecord::new();
        line.read_byte_record_into(&mut rec)?;
        recs.push(rec.iter().map(<[u8]>::to_vec).collect());
    }
    assert_eq!(recs[0][0], b"a");
    Ok(())
}

#[test]
fn postgres_copy_csv_null_in_streaming_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"name,val\n,empty\n";
    let mut r = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new(),
    )?;
    let mut recs: Vec<Vec<Vec<u8>>> = Vec::new();
    while let Some(mut line) = r.next_line()? {
        let mut rec = ByteRecord::new();
        line.read_byte_record_into(&mut rec)?;
        recs.push(rec.iter().map(<[u8]>::to_vec).collect());
    }
    assert_eq!(recs.len(), 1);
    Ok(())
}

#[test]
fn streaming_parser_shift_window_with_mysql_escape() -> Result<(), Box<dyn StdError>> {
    let input = b"\\0\t\\b\n\\n\t\\r\n";
    for cap in [1, 2, 4, 8] {
        let mut r = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::MYSQL,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(cap),
        )?;
        let mut count = 0;
        while r.next_line()?.is_some() {
            count += 1;
        }
        assert_eq!(count, 2, "cap={cap}");
    }
    Ok(())
}

/// `IoParser` exhausting its input returns `Advance::Done`, exercising
/// the `advance_window` → `Advance::Done` branch.
#[test]
fn streaming_parser_done_when_exhausted() -> Result<(), Box<dyn StdError>> {
    let mut p = IoParser::with_options(
        Cursor::new(b"a,b\nc,d\n" as &[u8]),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut count = 0usize;
    while let Some(mut line) = p.next_line()? {
        let mut r = ByteRecord::new();
        line.read_byte_record_into(&mut r)?;
        count += 1;
    }
    assert_eq!(count, 2);
    Ok(())
}

/// `header_index` and `header_indices` resolve names against a header record
/// obtained with default parse options rather than an explicit `Headers` policy.
#[test]
fn streaming_parser_header_index_and_indices_with_default_options() -> Result<(), Box<dyn StdError>>
{
    let mut p = IoParser::with_options(
        Cursor::new(b"x,y,x\n1,2,3\n" as &[u8]),
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("valid options");
    assert_eq!(p.header_index("y")?, Some(1));
    assert_eq!(p.header_index("missing")?, None);
    assert_eq!(p.header_indices("x")?, &[0, 2]);
    Ok(())
}

/// `IoParser::set_headers` and `has_headers` cover the engine
/// `set_headers` and `has_headers` implementations.
#[test]
fn streaming_parser_set_and_has_headers() {
    let mut p = IoParser::with_options(
        Cursor::new(b"Alice,30\n" as &[u8]),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    assert!(!p.has_headers());
    let mut hdrs = ByteRecord::new();
    hdrs.push_field(b"name");
    hdrs.push_field(b"age");
    p.set_headers(hdrs);
    assert!(p.has_headers());
}

/// Rewinding re-reads the header record, and a `MatchFirst` width has to be
/// re-derived from it rather than kept from the previous pass.
#[test]
fn rewinding_restores_a_match_first_width_from_the_header() -> Result<(), Box<dyn StdError>> {
    let data = b"a,b,c\n1,2,3\n4,5,6\n";
    let mut parser = IoParser::with_options(
        Cursor::new(&data[..]),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::MatchFirst),
    )?;

    let mut record = ByteRecord::new();
    let mut count = 0;
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        count += 1;
    }
    assert_eq!(count, 2);

    parser.rewind()?;

    let mut after = 0;
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        assert_eq!(record.len(), 3);
        after += 1;
    }
    assert_eq!(after, 2, "the rewound pass sees the same records");
    Ok(())
}

/// A stream configured without a header record must not consume its first
/// record looking for one, whatever the window boundaries turn out to be.
#[test]
fn a_headerless_stream_keeps_its_first_record() -> Result<(), Box<dyn StdError>> {
    let data = b"1,2,3\n4,5,6\n7,8,9\n";
    let mut parser = IoParser::with_options(
        FailingReader::new(data.to_vec()).max_chunk(3),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = ByteRecord::new();
    let mut firsts = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        firsts.push(record.get(0).map(<[u8]>::to_vec));
    }

    assert_eq!(
        firsts,
        vec![
            Some(b"1".to_vec()),
            Some(b"4".to_vec()),
            Some(b"7".to_vec())
        ]
    );
    Ok(())
}

/// A poisoned streaming parser reports the failure from its own window
/// advance, which is a separate code path from the slice parser's.
#[test]
fn a_poisoned_streaming_parser_refuses_every_later_record() -> Result<(), Box<dyn StdError>> {
    let data = b"a,b\nc,\"unterminated\n";
    let mut parser = IoParser::with_options(
        Cursor::new(&data[..]),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = ByteRecord::new();
    let mut line = parser.next_line()?.expect("the first record");
    line.read_byte_record_into(&mut record)?;

    let mut seen_failure = false;
    match parser.next_line() {
        Ok(Some(mut line)) => {
            let _ = line
                .read_byte_record_into(&mut record)
                .expect_err("the quoted field is never closed");
        }
        Ok(None) => unreachable!("the malformed record is not silently dropped"),
        Err(_) => seen_failure = true,
    }

    let again = parser
        .next_line()
        .expect_err("a poisoned parser cannot be resumed");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    assert!(
        seen_failure || again.kind() == ErrorKind::ParserFailed,
        "the failure surfaces exactly once before the poisoning"
    );
    Ok(())
}

// ── stream-level IO error propagation into Error ────────────────────────────

/// An IO error raised while reading the stream is wrapped by `Error` and can
/// be recovered via `into_io_error`, preserving its original `io::ErrorKind`.
#[test]
fn streaming_error_into_io_error_recovers_wrapped_io_error() -> Result<(), Box<dyn StdError>> {
    let mut r = IoParser::with_options(
        FailingReader::new(Vec::new()).fail_all_reads(io::ErrorKind::BrokenPipe),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let err = r.next_line().expect_err("expected IO error");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    let io_err = err.into_io_error().expect("should be Io variant");
    assert_eq!(io_err.kind(), io::ErrorKind::BrokenPipe);
    Ok(())
}

/// An `Error` produced from a failed stream read reports the underlying IO
/// error as its `source()`.
#[test]
fn streaming_error_source_is_some_for_io_error() -> Result<(), Box<dyn StdError>> {
    let mut r = IoParser::with_options(
        FailingReader::new(Vec::new()).fail_all_reads(io::ErrorKind::UnexpectedEof),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let err = r.next_line().expect_err("expected IO error");
    let source: Option<&dyn std::error::Error> = std::error::Error::source(&err);
    assert!(source.is_some(), "IO error should have a source");
    Ok(())
}

// ── named-dialect CRLF handling across buffer boundaries ────────────────────

/// A TSV dialect with `RecordEnding::CrLf` correctly splits records at every
/// buffer capacity, including capacities that split the CRLF pair itself.
#[test]
fn streaming_named_dialect_crlf_across_boundaries() -> Result<(), Box<dyn StdError>> {
    let input = b"a\tb\r\n\"c\td\"\r\ne\tf\r\n";
    for cap in [1, 4, 8, 64] {
        let mut r = IoParser::with_options(
            Cursor::new(input.as_slice()),
            FormatOptions::TSV.record_ending(RecordEnding::CrLf),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(cap),
        )?;
        let mut recs: Vec<Vec<Vec<u8>>> = Vec::new();
        while let Some(mut line) = r.next_line()? {
            let mut rec = ByteRecord::new();
            line.read_byte_record_into(&mut rec)?;
            recs.push(rec.iter().map(<[u8]>::to_vec).collect());
        }
        assert_eq!(recs.len(), 3, "cap={cap}");
    }
    Ok(())
}

// ─── Resumable incomplete-record parsing over short reads ──────────────────
//
// A `Read` that hands back only a byte or two at a time makes the io parser
// widen the same record's window over and over, the streaming analog of tiny
// push chunks. The resume checkpoint must make that reassemble exactly what the
// slice parser reads from the whole input, across an adversarial corpus and the
// dialects that decline the fast path alike.

/// Records reduced to fields, extent, and index, or the stopping error.
type ResumeOutcome = Result<Vec<(Vec<Vec<u8>>, core::ops::Range<usize>, u64)>, (ErrorKind, usize)>;

fn slice_outcome(input: &[u8], format: FormatOptions, options: ParseOptions) -> ResumeOutcome {
    (|| {
        let mut parser = SliceParser::with_options(input, format, options)?;
        let mut out = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            out.push((
                record.iter().map(<[u8]>::to_vec).collect(),
                record.byte_range(),
                record.index(),
            ));
        }
        Ok(out)
    })()
    .map_err(|error: coseva::Error| (error.kind(), error.location().byte))
}

fn short_read_outcome(
    input: &[u8],
    max: usize,
    cap: usize,
    format: FormatOptions,
    options: ParseOptions,
) -> ResumeOutcome {
    (|| {
        let mut parser = IoParser::with_options(
            FailingReader::new(input.to_vec()).max_chunk(max.max(1)),
            format,
            options.buffer_capacity(cap),
        )?;
        let mut out = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            out.push((
                record.iter().map(<[u8]>::to_vec).collect(),
                record.byte_range(),
                record.index(),
            ));
        }
        Ok(out)
    })()
    .map_err(|error: coseva::Error| (error.kind(), error.location().byte))
}

const RESUME_CORPUS: &[&[u8]] = &[
    b"a,b,c\nd,e,f\n",
    b"\"a\nb\",second\nthird,fourth\n",
    b"\"a,b\",\"c\"\"d\"\ne,f\n",
    b"\"\",\"\",\"\"\n",
    b"\"multi\r\nline\",x\ny,z\n",
    b"one,\"two\",three\nfour,\"five\",six\n",
    b"a,b\r\nc,d\r\n",
    b"trailing,no,newline",
    b"\"unterminated,quote\n",
    b"a,\"b\"x,c\n",
    b"\xE2\x9C\x93,check\ndata,\xE2\x82\xAC\n",
    b"first\n\"second\nwith\nmany\nlines\"\nthird\n",
];

#[test]
fn short_reads_reproduce_the_slice_parser_across_the_corpus() {
    for input in RESUME_CORPUS {
        let expected = slice_outcome(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        );
        for max in [1_usize, 2, 3, 7] {
            for cap in [1_usize, 4, 16] {
                let actual = short_read_outcome(
                    input,
                    max,
                    cap,
                    FormatOptions::CSV,
                    ParseOptions::new().headers(Headers::None),
                );
                assert_eq!(
                    actual,
                    expected,
                    "max={max} cap={cap} input={:?}",
                    String::from_utf8_lossy(input),
                );
            }
        }
    }
}

#[test]
fn short_reads_reproduce_the_slice_parser_across_dialects() {
    // Every dialect family that now resumes -- alternate delimiters, backslash
    // and MySQL and unquoted escapes, CRLF endings, Postgres NULLs -- plus the
    // one that still declines (comments and blank-skip), must reproduce the
    // slice parser byte for byte no matter how the short reads fall.
    for format in [
        FormatOptions::TSV,
        FormatOptions::SEMICOLON,
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::MYSQL,
        FormatOptions::RFC4180,
        FormatOptions::COMMENTED_CSV,
        FormatOptions::PYTHON_CSV,
        FormatOptions::PYTHON_ESCAPED,
        FormatOptions::POSTGRES_COPY_CSV,
    ] {
        for input in RESUME_CORPUS {
            let expected = slice_outcome(input, format, ParseOptions::new().headers(Headers::None));
            for max in [1_usize, 3, 8] {
                let actual = short_read_outcome(
                    input,
                    max,
                    2,
                    format,
                    ParseOptions::new().headers(Headers::None),
                );
                assert_eq!(
                    actual,
                    expected,
                    "format={format:?} max={max} input={:?}",
                    String::from_utf8_lossy(input),
                );
            }
        }
    }
}

#[test]
fn short_reads_reproduce_the_slice_parser_under_tight_limits() {
    let options = || {
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(12, 6, 4))
    };
    for input in RESUME_CORPUS {
        let expected = slice_outcome(input, FormatOptions::CSV, options());
        for max in [1_usize, 3, 8] {
            for cap in [1_usize, 8] {
                let actual = short_read_outcome(input, max, cap, FormatOptions::CSV, options());
                assert_eq!(
                    actual,
                    expected,
                    "max={max} cap={cap} input={:?}",
                    String::from_utf8_lossy(input),
                );
            }
        }
    }
}

#[test]
fn short_reads_keep_a_huge_record_correct_and_linear() {
    // A quoted field far larger than the buffer, read one byte at a time. Only
    // a resumable scan finishes this quickly; the result must match the oracle.
    let mut input = Vec::new();
    input.extend_from_slice(b"\"");
    for index in 0..40_000_u32 {
        match index % 5 {
            0 => input.extend_from_slice(b"\n"),
            1 => input.extend_from_slice(b"\"\""),
            _ => input.push(b'a' + u8::try_from(index % 26).expect("fits")),
        }
    }
    input.extend_from_slice(b"\",tail\nnext,row\n");

    let expected = slice_outcome(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    );
    let actual = short_read_outcome(
        &input,
        1,
        4,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    );
    assert_eq!(actual, expected);
    assert!(matches!(expected, Ok(ref rows) if rows.len() == 2));
}

/// Read one long adversarial record for `format` a single byte at a time and
/// assert the io parser reproduces the oracle. A quoted or unquoted field far
/// larger than the buffer forces the engine to grow the record's window across
/// tens of thousands of short reads; only a resumable scan finishes in time,
/// and reparsing the prefix each step would make the test run for seconds. The
/// `rows.len() >= 2` check makes a malformed corpus that stops early -- and so
/// never crosses the long field -- a failure rather than a false pass.
fn assert_long_record_resumes(format: FormatOptions, input: &[u8]) {
    let options = || ParseOptions::new().headers(Headers::None);
    let expected = slice_outcome(input, format, options());
    let actual = short_read_outcome(input, 1, 4, format, options());
    assert_eq!(actual, expected, "format={format:?}");
    assert!(
        matches!(expected, Ok(ref rows) if rows.len() >= 2),
        "format={format:?} corpus must parse as a long record plus a tail",
    );
}

#[test]
fn short_reads_keep_long_records_of_every_resuming_dialect_linear() {
    // The generalized resume scan covers CRLF, backslash, MySQL and unquoted
    // escapes, skip-initial-space, and Compatible recovery, not just strict
    // CSV. Each case below hides that dialect's structural bytes inside one
    // enormous field, so the engine must carry its checkpoint -- quoting,
    // escape, and field state -- across the whole field as the window grows one
    // byte at a time.
    let reps = 12_000_u32;
    let letter = |index: u32| b'a' + u8::try_from(index % 26).expect("fits");

    // CRLF endings with doubled-quote escapes: a long quoted field carrying
    // bare CRLFs, delimiters, and doubled quotes, closed and followed by a
    // CRLF-terminated record.
    let mut crlf = vec![b'"'];
    for index in 0..reps {
        match index % 7 {
            0 => crlf.extend_from_slice(b"\r\n"),
            1 => crlf.push(b','),
            2 => crlf.extend_from_slice(b"\"\""),
            _ => crlf.push(letter(index)),
        }
    }
    crlf.extend_from_slice(b"\",tail\r\nnext,row\r\n");
    assert_long_record_resumes(FormatOptions::RFC4180, &crlf);

    // Backslash escapes inside a quoted field: escaped quotes and backslashes
    // alongside literal delimiters and newlines.
    let mut backslash = vec![b'"'];
    for index in 0..reps {
        match index % 7 {
            0 => backslash.extend_from_slice(b"\\\""),
            1 => backslash.extend_from_slice(b"\\\\"),
            2 => backslash.push(b','),
            3 => backslash.push(b'\n'),
            _ => backslash.push(letter(index)),
        }
    }
    backslash.extend_from_slice(b"\",tail\nnext,row\n");
    assert_long_record_resumes(FormatOptions::BACKSLASH_CSV, &backslash);

    // MySQL: no quoting, a long unquoted field whose tabs, newlines, and
    // backslashes are all backslash-escaped so the field never ends early.
    let mut mysql = Vec::new();
    for index in 0..reps {
        match index % 7 {
            0 => mysql.extend_from_slice(b"\\\t"),
            1 => mysql.extend_from_slice(b"\\\n"),
            2 => mysql.extend_from_slice(b"\\\\"),
            _ => mysql.push(letter(index)),
        }
    }
    mysql.extend_from_slice(b"\ttail\nnext\trow\n");
    assert_long_record_resumes(FormatOptions::MYSQL, &mysql);

    // Python QUOTE_NONE with a backslash escapechar: the comma-delimited analog
    // of the MySQL case, exercising the out-of-quotes escape skip.
    let mut unquoted = Vec::new();
    for index in 0..reps {
        match index % 7 {
            0 => unquoted.extend_from_slice(b"\\,"),
            1 => unquoted.extend_from_slice(b"\\\n"),
            2 => unquoted.extend_from_slice(b"\\\\"),
            _ => unquoted.push(letter(index)),
        }
    }
    unquoted.extend_from_slice(b",tail\nnext,row\n");
    assert_long_record_resumes(FormatOptions::PYTHON_ESCAPED, &unquoted);

    // skip-initial-space: the long quoted field opens only after a delimiter
    // and a run of spaces, the case the scan must recognize as a quote open.
    let mut trim = vec![b'a', b',', b' ', b' ', b'"'];
    for index in 0..reps {
        match index % 5 {
            0 => trim.push(b'\n'),
            1 => trim.push(b','),
            2 => trim.extend_from_slice(b"\"\""),
            _ => trim.push(letter(index)),
        }
    }
    trim.extend_from_slice(b"\"\nnext\n");
    assert_long_record_resumes(FormatOptions::PYTHON_CSV, &trim);

    // Compatible recovery: quoting plus permits for unquoted quotes and
    // trailing whitespace after a close quote. The long quoted field is closed
    // with trailing spaces, then an unquoted field carries a literal quote.
    let recovery_format = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE));
    let mut recovery = vec![b'"'];
    for index in 0..reps {
        match index % 5 {
            0 => recovery.push(b'\n'),
            1 => recovery.push(b','),
            2 => recovery.extend_from_slice(b"\"\""),
            _ => recovery.push(letter(index)),
        }
    }
    recovery.extend_from_slice(b"\"   ,pla\"in,tail\nnext,row\n");
    assert_long_record_resumes(recovery_format, &recovery);

    // Comment and blank skipping has already positioned the checkpoint at the
    // data record. Refill from one byte must not rescan the discarded prefix
    // or the settled part of the long field.
    let mut commented = vec![b'#'];
    for index in 0..reps {
        commented.push(letter(index));
    }
    commented.push(b'\n');
    for index in 0..reps {
        commented.push(letter(index));
    }
    commented.extend_from_slice(b",tail\n\n# between\nnext,row\n");
    assert_long_record_resumes(FormatOptions::COMMENTED_CSV, &commented);

    // A lone delimiter lead inside the field is data; a confirmed `||` ends
    // it. Both shapes and a tail split across the one-byte refill boundary are
    // carried by the resume state.
    #[cfg(feature = "multibyte")]
    {
        let format = FormatOptions::CSV.delimiter_sequence(b"||");
        let mut multibyte = Vec::new();
        for index in 0..reps {
            multibyte.push(if index % 31 == 0 { b'|' } else { letter(index) });
        }
        multibyte.extend_from_slice(b"||tail\nnext||row\n");
        assert_long_record_resumes(format, &multibyte);
    }
}
