//! Tests for the push parser.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;

use coseva::{Chunk, PushParser, TextRecord};

/// Lend a whole chunk to the parser, returning every record it completed.
///
/// A chunk that ends inside a record leaves the tail with the parser, so the
/// loan is repeated from the reported offset until the chunk is exhausted.
fn push_chunk<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    chunk: &[u8],
) -> Result<Vec<ByteRecord>, coseva::Error> {
    let mut records = Vec::new();
    let mut offset = 0;
    loop {
        let mut guard = parser.chunk(&chunk[offset..]);
        let result = drain_chunk(&mut guard, &mut records);
        offset += guard.done();
        result?;
        if offset >= chunk.len() {
            return Ok(records);
        }
    }
}

/// Declare the stream complete, returning the records that releases.
///
/// The last record of a stream can only terminate once the stream is known to
/// be over, so an empty chunk is lent afterwards to read it out.
fn push_finish<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
) -> Result<Vec<ByteRecord>, coseva::Error> {
    parser.finish();
    let mut records = Vec::new();
    let mut guard = parser.chunk(b"");
    let result = drain_chunk(&mut guard, &mut records);
    drop(guard);
    result?;
    Ok(records)
}

/// Collect every record the lent chunk completes.
fn drain_chunk<F: coseva::format::CsvFormat>(
    guard: &mut Chunk<'_, '_, F>,
    records: &mut Vec<ByteRecord>,
) -> Result<(), coseva::Error> {
    while let Some(mut line) = guard.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        records.push(record);
    }
    Ok(())
}

/// Lend one whole chunk to the parser, reporting how much of it was taken.
///
/// Records the chunk completes are read and discarded, so this is for tests
/// that care about acceptance rather than about the records themselves.
fn lend<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
) -> Result<usize, coseva::Error> {
    let mut records = Vec::new();
    let mut guard = parser.chunk(input);
    let result = drain_chunk(&mut guard, &mut records);
    let taken = guard.done();
    result?;
    Ok(taken)
}

/// Treat every record as data, bypassing the default first-record header policy.
fn unheaded_push() -> PushParser {
    PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
}
use coseva::ByteRecord;
use coseva::ErrorKind;
use coseva::Predicate;
use coseva::SliceParser;
use coseva::config::{
    BlankRecords, FieldCount, FormatOptions, Headers, Limits, ParseOptions, ReadBom, RecordEnding,
    Recovery, Syntax,
};
use coseva::format::Csv;

#[test]
fn push_parser_handles_every_split_point() -> Result<(), Box<dyn StdError>> {
    let input = b"a,\"b\nc\"\nd,\"e\"\"f\"";
    for split in 0..=input.len() {
        let mut parser = unheaded_push();
        let mut records = push_chunk(&mut parser, &input[..split])?;
        records.extend(push_chunk(&mut parser, &input[split..])?);
        records.extend(push_finish(&mut parser)?);
        assert_eq!(records.len(), 2, "split {split}");
        assert_eq!(records[0].index(), 0);
        assert_eq!(records[1].index(), 1);
        assert_eq!(records[0].get(1), Some(b"b\nc".as_slice()));
        assert_eq!(records[1].get(1), Some(b"e\"f".as_slice()));
    }
    Ok(())
}

#[test]
fn direct_owned_reads_handle_every_split_and_unterminated_tail() -> Result<(), Box<dyn StdError>> {
    let input = b"a,\"b\nc\"\nd,\"e\"\"f\"";
    for split in 0..=input.len() {
        let pieces = [&input[..split], &input[split..]];

        let mut bytes_parser = unheaded_push();
        let mut byte_record = ByteRecord::new();
        let mut byte_records = Vec::new();
        for piece in pieces {
            let mut offset = 0;
            while offset < piece.len() {
                let mut chunk = bytes_parser.chunk(&piece[offset..]);
                while chunk.read_byte_record_into(&mut byte_record)? {
                    byte_records.push(byte_record.clone());
                }
                offset += chunk.done();
            }
        }
        bytes_parser.finish();
        let mut chunk = bytes_parser.chunk(b"");
        while chunk.read_byte_record_into(&mut byte_record)? {
            byte_records.push(byte_record.clone());
        }
        let _ = chunk.done();

        let mut text_parser = unheaded_push();
        let mut text_record = TextRecord::new();
        let mut text_records = Vec::new();
        for piece in pieces {
            let mut offset = 0;
            while offset < piece.len() {
                let mut chunk = text_parser.chunk(&piece[offset..]);
                while chunk.read_text_record_into(&mut text_record)? {
                    text_records.push(text_record.clone());
                }
                offset += chunk.done();
            }
        }
        text_parser.finish();
        let mut chunk = text_parser.chunk(b"");
        while chunk.read_text_record_into(&mut text_record)? {
            text_records.push(text_record.clone());
        }
        let _ = chunk.done();

        assert_eq!(byte_records.len(), 2, "byte split {split}");
        assert_eq!(byte_records[0].get(1), Some(b"b\nc".as_slice()));
        assert_eq!(byte_records[1].get(1), Some(b"e\"f".as_slice()));
        assert_eq!(byte_records[0].byte_range(), 0..8);
        assert_eq!(byte_records[1].byte_range(), 8..16);
        assert_eq!(text_records.len(), 2, "text split {split}");
        assert_eq!(text_records[0].get(1), Some("b\nc"));
        assert_eq!(text_records[1].get(1), Some("e\"f"));
        assert_eq!(text_records[0].byte_range(), 0..8);
        assert_eq!(text_records[1].byte_range(), 8..16);
    }
    Ok(())
}

#[test]
fn direct_escaped_reads_handle_every_structural_split() -> Result<(), Box<dyn StdError>> {
    const ROW: &[u8] = b"\"Bo\"\"ton\",\"Ma\"\"sachusetts\",4500000,42.3601,-71.0589,true\n";
    let input = ROW.repeat(3);

    for split in 0..=input.len() {
        let mut parser = unheaded_push();
        let mut record = ByteRecord::new();
        let mut records = Vec::new();
        for piece in [&input[..split], &input[split..]] {
            let mut offset = 0;
            while offset < piece.len() {
                let mut chunk = parser.chunk(&piece[offset..]);
                while chunk.read_byte_record_into(&mut record)? {
                    records.push(record.clone());
                }
                offset += chunk.done();
            }
        }
        parser.finish();
        let mut chunk = parser.chunk(b"");
        while chunk.read_byte_record_into(&mut record)? {
            records.push(record.clone());
        }
        let _ = chunk.done();

        assert_eq!(records.len(), 3, "split {split}");
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.get(0), Some(&b"Bo\"ton"[..]), "split {split}");
            assert_eq!(record.get(1), Some(&b"Ma\"sachusetts"[..]), "split {split}");
            assert_eq!(
                record.byte_range(),
                index * ROW.len()..(index + 1) * ROW.len(),
                "split {split}"
            );
        }
    }
    Ok(())
}

#[test]
fn direct_text_reads_resolve_headers_before_fused_data_reads() -> Result<(), Box<dyn StdError>> {
    let input = b"name,value\nalpha,1\nbeta,2";
    let mut parser = PushParser::<Csv>::new(ParseOptions::new())?;
    let mut record = TextRecord::new();
    let mut rows = Vec::new();

    let mut chunk = parser.chunk(input);
    while chunk.read_text_record_into(&mut record)? {
        rows.push(record.clone());
    }
    assert_eq!(chunk.done(), input.len());
    parser.finish();
    let mut chunk = parser.chunk(b"");
    while chunk.read_text_record_into(&mut record)? {
        rows.push(record.clone());
    }
    let _ = chunk.done();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].iter().collect::<Vec<_>>(), ["alpha", "1"]);
    assert_eq!(rows[1].iter().collect::<Vec<_>>(), ["beta", "2"]);
    assert_eq!(rows[0].byte_range(), 11..19);
    assert_eq!(rows[1].byte_range(), 19..25);
    Ok(())
}

#[test]
fn unheaded_push_parser_settles_headers_from_a_growable_window() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_push();
    let mut record = ByteRecord::new();
    let mut guard = parser.chunk(b"alpha,beta\n");
    // The record ends on a terminator at the very edge of the chunk, which is
    // enough to know it is whole, so it is lent without waiting for more.
    guard
        .next_line()?
        .expect("record")
        .read_byte_record_into(&mut record)?;
    assert!(guard.next_line()?.is_none());
    assert_eq!(guard.done(), b"alpha,beta\n".len());
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"alpha".as_slice(), b"beta".as_slice()]
    );

    parser.finish();
    let mut guard = parser.chunk(b"");
    assert!(guard.next_line()?.is_none());
    Ok(())
}

#[test]
fn push_parser_reprobes_an_overfull_unfinished_record_for_its_real_error() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(3, 100, 16)),
    )
    .expect("valid options");

    // The window has to be brought up against the limit before the chunk that
    // overflows it arrives, because only then is the record reprobed rather
    // than read straight out of the caller's bytes.
    assert_eq!(lend(&mut parser, b"ab").expect("window fills"), 2);
    let error = lend(&mut parser, b"\"ce")
        .expect_err("probing the overfull record should find the syntax error");

    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().byte, 2);
}

#[test]
fn push_parser_returns_completed_records_before_a_later_error() {
    let mut parser = unheaded_push();
    let input = b"valid,row\ninvalid\"field,row\n";
    // Records completed before the malformed one are delivered first, and the
    // error is reported by the call that reaches it.
    let mut records = Vec::new();
    let mut guard = parser.chunk(input);
    let error = drain_chunk(&mut guard, &mut records).expect_err("later row should fail");
    drop(guard);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].iter().collect::<Vec<_>>(),
        [b"valid".as_slice(), b"row".as_slice()],
    );
    assert_eq!(error.kind(), coseva::ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().byte, 17);
    assert_eq!(error.location().line, 2);
    assert_eq!(
        push_finish(&mut parser)
            .expect_err("parser should remain failed")
            .kind(),
        coseva::ErrorKind::ParserFailed,
    );
}

#[test]
fn push_parser_tracks_quoted_crlf_and_custom_terminator_lines() {
    let input = b"first,\"a\nb\"\r\nsecond,row\nbad\"quote,x";
    for split in 0..=input.len() {
        let mut parser = unheaded_push();
        let mut error = None;
        for chunk in [&input[..split], &input[split..]] {
            if let Err(found) = push_chunk(&mut parser, chunk) {
                error = Some(found);
                break;
            }
        }
        let error = match error {
            Some(error) => error,
            None => push_finish(&mut parser).expect_err("third record should fail"),
        };
        assert_eq!(error.kind(), coseva::ErrorKind::UnexpectedQuote);
        assert_eq!(error.location().line, 4, "split {split}");
    }

    let format = FormatOptions::CSV
        .delimiter(b',')
        .quote(b'"')
        .record_ending(coseva::config::RecordEnding::Byte(b'|'))
        .escape(coseva::config::Escape::DoubleQuote);
    let mut parser = PushParser::with_options(
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT),
    )
    .expect("valid options");
    let mut records = Vec::new();
    let mut guard = parser.chunk(b"a\nb|bad\"quote|");
    let error = drain_chunk(&mut guard, &mut records).expect_err("second record should fail");
    drop(guard);
    assert_eq!(records.len(), 1);
    assert_eq!(error.location().line, 2);

    let format = FormatOptions::CSV.comment(Some(b'#'));
    let mut parser = PushParser::with_options(
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT),
    )
    .expect("valid options");
    let mut records = Vec::new();
    let mut guard = parser.chunk(b"# ignored\n\nbad\"quote,x");
    let error = drain_chunk(&mut guard, &mut records)
        .expect_err("malformed data after ignored lines should fail");
    drop(guard);
    assert_eq!(records.len(), 1, "the blank line before it is a record");
    assert_eq!(error.location().line, 3);
}

#[test]
fn compatible_push_parser_matches_slice_reader() -> Result<(), Box<dyn StdError>> {
    let input = b"a,\"b,c\",d\"e\nnext,row,last\n";
    let syntax = Syntax::Compatible(Recovery::PERMISSIVE);
    let mut expected = SliceParser::with_options(
        input,
        FormatOptions::CSV.syntax(syntax),
        ParseOptions::new().headers(coseva::config::Headers::None),
    )?;
    let mut expected_rows = Vec::new();
    while let Some(mut line) = expected.next_line()? {
        let mut row = ByteRecord::new();
        line.read_byte_record_into(&mut row)?;
        expected_rows.push(row);
    }

    for split in 0..=input.len() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV.syntax(syntax),
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::DEFAULT),
        )
        .expect("valid options");
        let mut actual = push_chunk(&mut parser, &input[..split])?;
        actual.extend(push_chunk(&mut parser, &input[split..])?);
        actual.extend(push_finish(&mut parser)?);
        assert_eq!(actual, expected_rows, "split {split}");
    }
    Ok(())
}

#[test]
fn push_parser_applies_complete_named_presets_at_every_split() -> Result<(), Box<dyn StdError>> {
    let cases = [
        (
            FormatOptions::PYTHON_CSV,
            b"  first,   \"quoted\",  tail  \n".as_slice(),
            vec![vec![
                b"  first".to_vec(),
                b"quoted".to_vec(),
                b"tail  ".to_vec(),
            ]],
        ),
        (
            FormatOptions::TRIMMED_CSV,
            b"  first  ,\"  quoted  \"\n".as_slice(),
            vec![vec![b"first".to_vec(), b"quoted".to_vec()]],
        ),
        (
            FormatOptions::COMMENTED_CSV,
            b"# ignored\n\nvalue,row\n".as_slice(),
            vec![vec![b"value".to_vec(), b"row".to_vec()]],
        ),
    ];

    for (format, input, expected) in cases {
        for split in 0..=input.len() {
            let mut parser = PushParser::with_options(
                format,
                ParseOptions::new()
                    .headers(Headers::None)
                    .limits(Limits::DEFAULT),
            )
            .expect("valid options");
            let mut records = push_chunk(&mut parser, &input[..split])?;
            records.extend(push_chunk(&mut parser, &input[split..])?);
            records.extend(push_finish(&mut parser)?);
            let actual = records
                .iter()
                .map(|record| record.iter().map(<[u8]>::to_vec).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "format {format:?}, split {split}");
        }
    }
    Ok(())
}

#[test]
fn push_parser_consumes_headers_like_the_other_parsers() -> Result<(), Box<dyn StdError>> {
    let input = b"city,population\nBoston,650706\nLondon,8982000\n";
    for split in 0..=input.len() {
        let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
        let mut records = push_chunk(&mut parser, &input[..split])?;
        records.extend(push_chunk(&mut parser, &input[split..])?);
        records.extend(push_finish(&mut parser)?);

        assert_eq!(records.len(), 2, "header row is not a data record");
        assert_eq!(
            parser
                .headers()
                .map(|record| record.get(0).map(<[u8]>::to_vec)),
            Some(Some(b"city".to_vec())),
            "split {split}"
        );
        assert_eq!(parser.header_index("population"), Some(1));
        assert_eq!(parser.header_index("missing"), None);
        assert!(parser.has_headers());
        // The header record still consumes index 0, matching SliceParser.
        assert_eq!(records[0].index(), 1);
        assert_eq!(records[0].get(0), Some(b"Boston".as_slice()));
        assert_eq!(records[1].get(0), Some(b"London".as_slice()));
    }
    Ok(())
}

#[test]
fn push_parser_enforces_field_count() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::Exact(2)),
    )?;
    // Records completed before the failure are delivered first, so the width
    // error is reported by the following call.
    let records = push_chunk(&mut parser, b"a,b\nc,d,e\n")?;
    assert_eq!(records.len(), 1);
    let error = push_finish(&mut parser).expect_err("the second record has three fields");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3
        }
    );
    Ok(())
}

#[test]
fn push_parser_strips_a_bom_split_across_chunks() -> Result<(), Box<dyn StdError>> {
    let input = b"\xEF\xBB\xBFcity,population\nBoston,650706\n";
    for split in 0..=input.len() {
        let mut parser = unheaded_push();
        let mut records = push_chunk(&mut parser, &input[..split])?;
        records.extend(push_chunk(&mut parser, &input[split..])?);
        records.extend(push_finish(&mut parser)?);
        assert_eq!(records.len(), 2, "split {split}");
        assert_eq!(
            records[0].get(0),
            Some(b"city".as_slice()),
            "BOM should not survive in the first field at split {split}"
        );
    }
    Ok(())
}

#[test]
fn push_parser_rejects_a_bom_when_configured() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV.read_bom(coseva::config::ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let error =
        lend(&mut parser, b"\xEF\xBB\xBFcity,population\n").expect_err("a leading BOM is rejected");
    assert_eq!(error.kind(), ErrorKind::RejectedBom);
    Ok(())
}

#[test]
fn push_parser_reports_stream_absolute_positions() -> Result<(), Box<dyn StdError>> {
    let input = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";

    // The slice parser defines the ground truth for offsets and locations.
    let mut slice = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut expected = Vec::new();
    loop {
        let read = match slice.next_line()? {
            Some(mut line) => {
                line.record()?;
                true
            }
            None => false,
        };
        if !read {
            break;
        }
        let location = slice.location();
        expected.push((slice.location().byte, location.line, location.record));
    }

    for chunk in [1_usize, 5, 13, 1024] {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )?;
        let mut actual = Vec::new();
        let mut fed = 0;
        while fed < input.len() {
            let end = (fed + chunk).min(input.len());
            // The parser's location can only be read once the loan has ended,
            // so the chunk is lent one record at a time.
            while fed < end {
                let mut guard = parser.chunk(&input[fed..end]);
                let read = match guard.next_line()? {
                    Some(mut line) => {
                        line.record()?;
                        true
                    }
                    None => false,
                };
                fed += guard.done();
                if !read {
                    break;
                }
                let location = parser.location();
                actual.push((parser.location().byte, location.line, location.record));
            }
            fed = end;
        }
        parser.finish();
        loop {
            let mut guard = parser.chunk(b"");
            let read = match guard.next_line()? {
                Some(mut line) => {
                    line.record()?;
                    true
                }
                None => false,
            };
            drop(guard);
            if !read {
                break;
            }
            let location = parser.location();
            actual.push((parser.location().byte, location.line, location.record));
        }
        assert_eq!(actual, expected, "positions diverged at chunk size {chunk}");
        assert!(parser.is_done(), "chunk size {chunk}");
        assert_eq!(
            parser.location().byte,
            input.len(),
            "final offset at chunk size {chunk}"
        );
        assert_eq!(
            parser.headers().and_then(|headers| headers.get(1)),
            Some(b"country".as_slice()),
            "headers at chunk size {chunk}"
        );
        assert_eq!(parser.header_index("city"), Some(0));
        assert_eq!(parser.header_indices("country"), &[1]);
        assert!(parser.has_headers());
    }
    Ok(())
}

#[test]
fn push_parser_reset_starts_an_unrelated_stream() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_push();
    let records = push_finish_with(&mut parser, b"a,b\n")?;
    assert_eq!(records.len(), 1);
    assert_eq!(parser.location().byte, 4);
    assert!(parser.is_done());

    parser.reset();
    assert_eq!(parser.location().byte, 0);
    assert!(!parser.is_done());

    let records = push_finish_with(&mut parser, b"c,d\ne,f")?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].get(0), Some(b"e".as_slice()));
    assert_eq!(parser.location().byte, 7);
    Ok(())
}

#[test]
fn push_parser_uses_provided_headers() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(FormatOptions::CSV, ParseOptions::new())?;
    let mut headers = ByteRecord::new();
    headers.push_field(b"city");
    headers.push_field(b"country");
    parser.set_headers(headers);

    let records = push_finish_with(&mut parser, b"Boston,US\n")?;
    assert_eq!(records.len(), 1, "the first record is data, not a header");
    assert_eq!(parser.header_index("country"), Some(1));
    assert_eq!(records[0].get(0), Some(b"Boston".as_slice()));
    Ok(())
}

/// Feed a whole chunk, finish the stream, and collect every record.
fn push_finish_with(
    parser: &mut PushParser,
    chunk: &[u8],
) -> Result<Vec<ByteRecord>, coseva::Error> {
    let mut records = push_chunk(parser, chunk)?;
    records.extend(push_finish(parser)?);
    Ok(records)
}

#[test]
fn push_parser_does_not_latch_field_count_from_a_partial_record() -> Result<(), Box<dyn StdError>> {
    // A record fed one byte at a time is speculatively parsed on every feed.
    // Those speculative parses must not settle `MatchFirst`'s width, or the
    // truncated prefix `a` would define it as one field and the whole record
    // `a,b` would then be rejected.
    let options = ParseOptions::new()
        .headers(Headers::None)
        .field_count(FieldCount::MatchFirst);

    let mut whole = PushParser::with_options(FormatOptions::CSV, options.clone())?;
    let expected = push_finish_with(&mut whole, b"a,b")?;
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].len(), 2);

    let mut split = PushParser::with_options(FormatOptions::CSV, options)?;
    let mut records = Vec::new();
    for byte in b"a,b" {
        records.extend(push_chunk(&mut split, &[*byte])?);
    }
    records.extend(push_finish(&mut split)?);

    assert_eq!(records, expected, "chunking changed the parse");
    Ok(())
}

// ── size limits and failure states ──────────────────────────────────────────────

/// Lending bytes right up against a tiny record limit either takes only part
/// of the chunk or reports the overflow on the next advance.
#[test]
fn push_parser_chunk_returns_partial_when_record_would_overflow() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(8, 8, 1024)),
    )?;
    // A caller that does not drain still has the loan bounded by the record
    // limit, so the second chunk can only be taken in part.
    let n = parser.chunk(b"abcd").done();
    assert_eq!(n, 4);
    let n2 = parser.chunk(b"efghij").done();
    assert!(n2 <= 6);
    Ok(())
}

/// A record whose unterminated bytes exceed the record limit is reported as
/// `RecordTooLarge`.
#[test]
fn push_parser_oversized_record_is_an_error() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(4, 4, 1024)),
    )
    .expect("valid options");
    // Lend limit + 2 bytes with no record ending so room drops to zero.
    let _ = lend(&mut parser, b"abcde");
    let err = lend(&mut parser, b"x").expect_err("record at limit should fail");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
}

/// Once a parser has failed, every further chunk is rejected rather than
/// resuming or corrupting state.
#[test]
fn push_parser_failed_parser_rejects_further_chunks() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(4, 4, 1024)),
    )?;
    let _ = lend(&mut parser, b"abcde");
    let _ = lend(&mut parser, b"x"); // poisons the parser
    let err = lend(&mut parser, b"more").expect_err("failed parser should reject further chunks");
    assert_eq!(err.kind(), ErrorKind::ParserFailed);
    Ok(())
}

// ── BOM handling ─────────────────────────────────────────────────────────────────

/// A leading BOM is rejected as soon as it is lent when the parser is
/// configured to reject BOMs.
#[test]
fn push_parser_bom_reject_fails_on_chunk() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let bom = b"\xEF\xBB\xBFa,b\n";
    let err = lend(&mut parser, bom).expect_err("rejected BOM should fail on the lent chunk");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
}

/// A BOM lent in one chunk is detected and stripped from the first field.
#[test]
fn push_parser_bom_detected_and_stripped() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV.read_bom(ReadBom::Detect),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut records = push_chunk(&mut parser, b"\xEF\xBB\xBFa,b\n")?;
    records.extend(push_finish(&mut parser)?);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].get(0), Some(b"a".as_slice()));
    Ok(())
}

/// Lending only part of a BOM prefix and then finishing must preserve the
/// incomplete prefix as ordinary field data.
#[test]
fn push_parser_bom_partial_then_finish() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV.read_bom(ReadBom::Detect),
        ParseOptions::new().headers(Headers::None),
    )?;
    lend(&mut parser, b"\xEF")?; // partial BOM prefix
    let records = push_finish(&mut parser)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].len(), 1);
    assert_eq!(records[0].get(0), Some(b"\xEF".as_slice()));
    Ok(())
}

// ── reset ────────────────────────────────────────────────────────────────────────

/// One observation from draining a push parser: a row with its position,
/// or the failure that stopped it.
#[derive(Debug, Eq, PartialEq)]
enum PushEvent {
    Row(Vec<Vec<u8>>, core::ops::Range<usize>, u64, u64),
    Failure(ErrorKind, u64, u64),
}

/// Lend `input` in `chunk`-sized pieces, draining each loan, and collect every
/// row or failure the parser produces.
fn drain_events<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    chunk: usize,
) -> Vec<PushEvent> {
    let mut events = Vec::new();
    let mut fed = 0;
    while fed < input.len() {
        let end = (fed + chunk).min(input.len());
        let mut offset = fed;
        while offset < end {
            let (taken, failed) = collect_events(parser, &input[offset..end], &mut events);
            offset += taken;
            if failed {
                // A failure must be sticky: a further loan reports it rather
                // than resuming.
                let mut guard = parser.chunk(b"");
                let error = guard.next_line().err().expect("the failure is sticky");
                assert_eq!(error.kind(), ErrorKind::ParserFailed);
                drop(guard);
                return events;
            }
        }
        fed = end;
    }
    parser.finish();
    let _ = collect_events(parser, b"", &mut events);
    events
}

/// Lend one chunk, recording every row it completes and the first failure it
/// reports, and hand back how much of the chunk the parser took.
fn collect_events<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    events: &mut Vec<PushEvent>,
) -> (usize, bool) {
    let mut guard = parser.chunk(input);
    let failed = loop {
        match guard.next_line() {
            Ok(Some(mut line)) => match line.record() {
                Ok(record) => events.push(PushEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    record.byte_range(),
                    record.index(),
                    0,
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(PushEvent::Failure(
                        error.kind(),
                        location.line,
                        location.record,
                    ));
                    break true;
                }
            },
            Ok(None) => break false,
            Err(error) => {
                let location = error.location();
                events.push(PushEvent::Failure(
                    error.kind(),
                    location.line,
                    location.record,
                ));
                break true;
            }
        }
    };
    (guard.done(), failed)
}

/// Draining a parser, resetting it, and replaying must be indistinguishable
/// from feeding a parser that was never used, including the positions and
/// the errors reported.
#[test]
fn a_reset_parser_behaves_exactly_like_a_fresh_one() {
    let inputs: [&[u8]; 6] = [
        b"city,pop\nBoston,650706\nLondon,8982000\n",
        b"a,b\n1,2\n3,4",
        b"a,b\n\"x,y\",\"q\"\"r\"\n",
        b"a,b\n1,2,3\n",
        b"a,b\n\"unterminated\n",
        b"",
    ];

    for chunk in [1, 3, 64] {
        for warmup in inputs {
            for replay in inputs {
                let mut fresh = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
                let expected = drain_events(&mut fresh, replay, chunk);

                let mut reused = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
                let _ = drain_events(&mut reused, warmup, chunk);
                reused.reset();
                let actual = drain_events(&mut reused, replay, chunk);

                assert_eq!(
                    actual, expected,
                    "reset after {warmup:?} diverged replaying {replay:?} in {chunk}-byte feeds",
                );
                assert_eq!(
                    reused.is_done(),
                    fresh.is_done(),
                    "reset after {warmup:?} left a different done state for {replay:?}",
                );
            }
        }
    }
}

/// `reset()` clears state so a new, unrelated stream can be lent.
#[test]
fn push_parser_reset_clears_state() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_push();
    lend(&mut parser, b"a,b\nc,d\n")?;
    push_finish(&mut parser)?;

    parser.reset();
    let mut records = push_chunk(&mut parser, b"x,y\n")?;
    records.extend(push_finish(&mut parser)?);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].get(0), Some(b"x".as_slice()));
    Ok(())
}

/// After a `reset`, lending the same bytes again yields the same records,
/// showing that internal buffers are reused rather than left dirty.
#[test]
fn push_parser_reset_reuses_buffers() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_push();
    let data = b"a,b\nc,d\n";
    let _ = push_chunk(&mut parser, data)?;
    parser.finish();
    parser.reset();

    let mut count = push_chunk(&mut parser, data)?.len();
    count += push_finish(&mut parser)?.len();
    assert_eq!(count, 2);
    Ok(())
}

/// `reset()` with `Headers::Provided` reinstalls the provided header record
/// for the next stream.
#[test]
fn push_parser_reset_with_provided_headers() -> Result<(), Box<dyn StdError>> {
    let mut headers = ByteRecord::new();
    headers.push_field(b"name");
    let options = ParseOptions::new().headers(Headers::Provided(headers));
    let mut parser = PushParser::with_options(FormatOptions::CSV, options).expect("valid options");
    let data = b"Alice\nBob\n";
    let _ = push_chunk(&mut parser, data)?;
    parser.finish();
    parser.reset();
    let _ = push_chunk(&mut parser, b"Carol\n")?;
    let _ = push_finish(&mut parser)?;
    Ok(())
}

/// Resetting reinstalls the configured headers, and a `MatchFirst` width has
/// to be re-derived from those headers before the next stream starts.
#[test]
fn resetting_a_push_parser_reinstalls_a_provided_header_width() -> Result<(), Box<dyn StdError>> {
    let headers = {
        let mut record = ByteRecord::new();
        record.push_field(b"a");
        record.push_field(b"b");
        record.push_field(b"c");
        record
    };

    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::Provided(headers))
            .field_count(FieldCount::MatchFirst),
    )?;

    let widths = |records: &[ByteRecord]| -> usize {
        for record in records {
            assert_eq!(record.len(), 3);
        }
        records.len()
    };

    let mut records = widths(&push_chunk(&mut parser, b"1,2,3\n4,5,6\n")?);
    records += widths(&push_finish(&mut parser)?);
    assert_eq!(records, 2);

    parser.reset();

    let mut reset = push_chunk(&mut parser, b"7,8,9\n")?;
    reset.extend(push_finish(&mut parser)?);
    assert_eq!(widths(&reset), 1, "the reset stream reuses the width");

    let mut widened = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::Provided({
                let mut record = ByteRecord::new();
                record.push_field(b"a");
                record.push_field(b"b");
                record
            }))
            .field_count(FieldCount::MatchFirst),
    )?;
    widened.finish();
    let mut guard = widened.chunk(b"1,2,3\n");
    let mut record = ByteRecord::new();
    let mut line = guard.next_line()?.expect("a record");
    let error = line
        .read_byte_record_into(&mut record)
        .expect_err("three fields do not match a two-field header");
    assert!(
        matches!(error.kind(), ErrorKind::FieldCountMismatch { .. }),
        "expected a width mismatch, got {error:?}"
    );
    Ok(())
}

// ── headers ──────────────────────────────────────────────────────────────────────

/// Before any chunk is lent, headers are unresolved and report as absent
/// rather than panicking.
#[test]
fn push_parser_headers_before_settling_returns_none() {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    assert!(parser.headers().is_none());
    let mut p2 = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    assert!(p2.header_index("city").is_none());
    assert!(p2.header_indices("city").is_empty());
}

/// Once the header record has been consumed, `headers`, `header_index`, and
/// `header_indices` report it.
#[test]
fn push_parser_headers_after_settling() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    // Lend two complete records so the first (Boston) ends strictly inside
    // the chunk.
    let mut guard = parser.chunk(b"city,pop\nBoston,1\nLondon,2\n");
    guard.next_line()?.expect("first data record");
    drop(guard);
    let headers = parser.headers().expect("headers after settlement");
    assert_eq!(headers.get(0), Some(b"city".as_slice()));
    assert_eq!(parser.header_index("city"), Some(0));
    assert_eq!(parser.header_indices("pop"), [1]);
    Ok(())
}

/// `header_index`, `header_indices`, and `set_headers` all agree once headers
/// have been established from the data stream.
#[test]
fn push_parser_header_api() -> Result<(), Box<dyn StdError>> {
    let mut parser =
        PushParser::with_options(FormatOptions::CSV, ParseOptions::new()).expect("valid options");
    // Lend header + two data rows.  A record ending exactly at the chunk
    // boundary looks truncated to the push engine; at least one completed
    // record followed by more bytes lets the engine commit the first record
    // and establish the headers.
    let _ = push_chunk(
        &mut parser,
        b"city,pop,city\nParis,2M,Paris\nTokyo,14M,Tokyo\n",
    )?;
    assert!(parser.has_headers());
    assert_eq!(parser.header_index("pop"), Some(1));
    assert_eq!(parser.header_indices("city"), &[0, 2]);
    // set_headers replaces the lookup.
    let mut hdrs = ByteRecord::new();
    hdrs.push_field(b"country");
    hdrs.push_field(b"iso");
    parser.set_headers(hdrs);
    assert_eq!(parser.header_index("iso"), Some(1));
    Ok(())
}

// ── pushdown filtering ───────────────────────────────────────────────────────────

/// A finished stream's `next_matching_line` finds a record later in the
/// stream.
#[test]
fn push_parser_advance_with_filter_finished_stream() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    parser.finish();
    let mut guard = parser.chunk(b"city,pop\nBoston,1\nLondon,2\n");
    let pred = Predicate::equals("city", "London");
    let found = guard.next_matching_line(&pred)?.is_some();
    assert!(found);
    Ok(())
}

/// A predicate naming a column absent from the headers matches nothing,
/// whether or not the stream has finished.
#[test]
fn push_parser_advance_with_filter_named_header_not_found() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    parser.finish();
    let mut guard = parser.chunk(b"city,pop\nBoston,1\n");
    let pred = Predicate::equals("missing_col", "Boston");
    let result = guard.next_matching_line(&pred)?;
    assert!(result.is_none());
    Ok(())
}

/// The same missing-column case is checked before the stream finishes too.
#[test]
fn push_parser_advance_with_filter_not_finished_column_name_not_found()
-> Result<(), Box<dyn StdError>> {
    // Lend 3 records so "Boston" ends strictly inside the chunk.
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let mut guard = parser.chunk(b"city,pop\nBoston,1\nLondon,2\n");
    let pred = Predicate::equals("missing_col", "Boston");
    let result = guard.next_matching_line(&pred)?;
    assert!(result.is_none());
    Ok(())
}

/// A finished parser that has already failed rejects `next_matching_line`
/// rather than resuming.
#[test]
fn push_parser_advance_with_filter_finished_already_failed() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(4, 4, 1024)),
    )
    .expect("valid options");
    // Trigger a failure (record too large).
    parser.finish();
    let mut guard = parser.chunk(b"abcde");
    while let Ok(Some(mut l)) = guard.next_line() {
        let _ = l.record();
    }
    drop(guard);
    // Parser is now failed.
    let pred = Predicate::equals(0, "x");
    let mut guard = parser.chunk(b"");
    let err = guard.next_matching_line(&pred).expect_err("failed parser");
    assert_eq!(err.kind(), ErrorKind::ParserFailed);
}

/// A malformed record encountered while filtering surfaces its error through
/// `next_matching_line` rather than being swallowed.
#[test]
fn push_parser_advance_with_filter_finished_parse_error() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    parser.finish();
    let mut guard = parser.chunk(b"good,1\n\"unclosed\n");
    let pred = Predicate::equals(0, "good");
    // First call should succeed (finds "good").
    let _ = guard.next_matching_line(&pred);
    // Second call hits the malformed record; the error may surface here.
    let pred2 = Predicate::equals(0, "unclosed");
    let _ = guard.next_matching_line(&pred2);
}

// ── done / default ───────────────────────────────────────────────────────────────

/// `is_done` reports pending data as not done, and finishing plus draining as
/// done.
#[test]
fn push_parser_is_done_after_finish_and_drain() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_push();
    lend(&mut parser, b"a,b\n")?;
    assert!(!parser.is_done());
    push_finish(&mut parser)?;
    assert!(parser.is_done());
    Ok(())
}

/// `PushParser::default()` compiles and behaves like `::new()`.
#[test]
fn push_parser_default_impl() {
    let _p = PushParser::default();
}

// ── chunk-boundary dialects ──────────────────────────────────────────────────────

/// A CRLF-terminated stream is parsed identically no matter where the chunk
/// boundary falls.
#[test]
fn push_parser_handles_crlf_dialect_across_chunk_boundaries() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.record_ending(RecordEnding::CrLf);
    let input = b"a,b\r\nc,d\r\n";
    for split in 0..=input.len() {
        let mut parser =
            PushParser::with_options(format, ParseOptions::new().headers(Headers::None))
                .expect("valid options");
        let mut records: Vec<ByteRecord> = Vec::new();
        for chunk in [&input[..split], &input[split..]] {
            records.extend(push_chunk(&mut parser, chunk)?);
        }
        records.extend(push_finish(&mut parser)?);
        assert_eq!(records.len(), 2, "split={split}");
        assert_eq!(records[0].iter().collect::<Vec<_>>(), [b"a", b"b"]);
        assert_eq!(records[1].iter().collect::<Vec<_>>(), [b"c", b"d"]);
    }
    Ok(())
}

/// The `MySQL` dialect's backslash escapes and NULL marker survive a chunk
/// boundary at any byte.
#[test]
fn push_parser_handles_mysql_dialect_across_chunk_boundaries() -> Result<(), Box<dyn StdError>> {
    let input = b"hel\\nlo\tworld\n\\N\tval\n";
    for split in 0..=input.len() {
        let mut parser = PushParser::with_options(
            FormatOptions::MYSQL,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options");
        let mut records: Vec<ByteRecord> = Vec::new();
        for chunk in [&input[..split], &input[split..]] {
            records.extend(push_chunk(&mut parser, chunk)?);
        }
        records.extend(push_finish(&mut parser)?);
        assert_eq!(records.len(), 2, "split={split}");
        assert_eq!(records[0].get(0), Some(b"hel\nlo".as_slice()));
    }
    Ok(())
}

/// A record split anywhere across a header-bearing stream still yields the
/// expected two data records.
#[test]
fn push_parser_exercises_advance_window_with_headers() -> Result<(), Box<dyn StdError>> {
    let input = b"name,age\nAlice,30\nBob,25\n";
    for split in 0..=input.len() {
        let mut parser = PushParser::with_options(FormatOptions::CSV, ParseOptions::new())
            .expect("valid options");
        let mut records: Vec<ByteRecord> = Vec::new();
        for chunk in [&input[..split], &input[split..]] {
            records.extend(push_chunk(&mut parser, chunk)?);
        }
        records.extend(push_finish(&mut parser)?);
        assert_eq!(records.len(), 2, "split={split}");
    }
    Ok(())
}

/// A blank record in the middle of the stream is skipped even when the chunks
/// are split around it.
#[test]
fn push_parser_blank_records_skip_across_chunk_boundaries() -> Result<(), Box<dyn StdError>> {
    let input = b"a,b\n\nc,d\n";
    let mut parser = PushParser::with_options(
        FormatOptions::CSV.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut guard = parser.chunk(input);
    let mut records: Vec<ByteRecord> = Vec::new();
    drain_chunk(&mut guard, &mut records)?;
    assert_eq!(guard.done(), input.len());
    records.extend(push_finish(&mut parser)?);
    assert_eq!(records.len(), 2);
    Ok(())
}

/// Lending data one to three bytes at a time still restores the cursor
/// correctly across a quoted field that spans a newline.
#[test]
fn push_parser_restores_cursor_on_truncated_window() -> Result<(), Box<dyn StdError>> {
    let input = b"\"multi\nline\"\ncontinuation\n";
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut records: Vec<ByteRecord> = Vec::new();
    for chunk in input.chunks(3) {
        records.extend(push_chunk(&mut parser, chunk)?);
    }
    records.extend(push_finish(&mut parser)?);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].get(0), Some(b"multi\nline".as_slice()));
    Ok(())
}

/// Lending many records in a single chunk still delivers every one, both
/// before and after `finish`.
#[test]
fn push_parser_processes_many_records() -> Result<(), Box<dyn StdError>> {
    let mut data = Vec::new();
    for i in 0..20u32 {
        data.extend_from_slice(format!("row{i},value{i}\n").as_bytes());
    }
    data.extend_from_slice(b"target,special\n");

    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut records = Vec::new();
    let mut guard = parser.chunk(&data);
    drain_chunk(&mut guard, &mut records)?;
    assert_eq!(guard.done(), data.len());

    let mut count = records.len();
    count += push_finish(&mut parser)?.len();
    assert_eq!(count, 21);
    Ok(())
}

// ─── Resumable incomplete-record parsing ───────────────────────────────────
//
// Feeding a record in tiny fixed-size chunks re-grows its window many times.
// The resume checkpoint must let the parser pick up where the previous window
// left off, reassembling exactly what the slice parser reads from the whole
// input, and it must stay correct for a record far larger than any one chunk.

/// A parse reduced to what a caller observes: each record's fields and index,
/// or the kind of the error that stopped it.
type Reduced = Result<Vec<(Vec<Vec<u8>>, u64)>, ErrorKind>;

/// The slice parser's reading of `input`, reduced to fields and index, or its
/// stopping error kind.
fn slice_reference(input: &[u8]) -> Reduced {
    (|| {
        let mut parser = SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )?;
        let mut out = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            out.push((record.iter().map(<[u8]>::to_vec).collect(), record.index()));
        }
        Ok(out)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

/// The push parser's reading of `input` fed in fixed-size chunks, reduced the
/// same way.
fn push_reference(input: &[u8], chunk_size: usize) -> Reduced {
    (|| {
        let mut parser = unheaded_push();
        let mut out = Vec::new();
        let push = |records: Vec<ByteRecord>, out: &mut Vec<(Vec<Vec<u8>>, u64)>| {
            for record in records {
                out.push((record.iter().map(<[u8]>::to_vec).collect(), record.index()));
            }
        };
        for chunk in input.chunks(chunk_size.max(1)) {
            push(push_chunk(&mut parser, chunk)?, &mut out);
        }
        push(push_finish(&mut parser)?, &mut out);
        Ok(out)
    })()
    .map_err(|error: coseva::Error| error.kind())
}

/// Every tiny chunk size reassembles what the slice parser read.
#[test]
fn push_parser_resume_matches_the_slice_parser_in_tiny_chunks() {
    let corpus: &[&[u8]] = &[
        b"a,b,c\nd,e,f\n",
        b"\"a\nb\",second\nthird,fourth\n",
        b"\"a,b\",\"c\"\"d\"\ne,f\n",
        b"one,\"two\",three\nfour,\"five\",six\n",
        b"trailing,no,newline",
        b"\"unterminated,quote\n",
        b"\"multi\r\nline\",x\ny,z\n",
    ];
    for input in corpus {
        let expected = slice_reference(input);
        for size in [1_usize, 2, 3, 5, 8] {
            let actual = push_reference(input, size);
            assert_eq!(
                actual,
                expected,
                "size={size} input={:?}",
                String::from_utf8_lossy(input),
            );
        }
    }
}

/// A quoted field much larger than any chunk, fed one byte at a time, still
/// reads out whole and matches the oracle.
#[test]
fn push_parser_resume_keeps_a_huge_record_correct() {
    let mut input = Vec::new();
    input.extend_from_slice(b"\"");
    for index in 0..30_000_u32 {
        match index % 4 {
            0 => input.extend_from_slice(b"\n"),
            1 => input.extend_from_slice(b"\"\""),
            _ => input.push(b'a' + u8::try_from(index % 26).expect("fits")),
        }
    }
    input.extend_from_slice(b"\",tail\nnext,row\n");

    let expected = slice_reference(&input);
    let actual = push_reference(&input, 1);
    assert_eq!(actual, expected);
    assert!(matches!(expected, Ok(ref rows) if rows.len() == 2));
}

/// A record-limit breach discovered while an undrained chunk settles is
/// reported with its limit and location at the next fallible call, rather than
/// being reduced to the generic latched failure.
#[test]
fn push_parser_reports_a_breach_discovered_while_settling() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(4, 4, 1024)),
    )
    .expect("valid options");
    // Leave a partial record in the window so the next chunk is absorbed
    // rather than borrowed, then never drain it: `settle` walks the tail into
    // the window, breaches the limit, and has no way to report it.
    lend(&mut parser, b"ab").expect("partial record is accepted");
    drop(parser.chunk(b"cdefghij"));
    let err = lend(&mut parser, b"k").expect_err("settled breach should surface");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
}
