//! Integration tests for delimiters and record terminators of several bytes.
//!
//! The point of interest throughout is the *partial* separator: a lone `|`
//! where the delimiter is `||`. That byte is what every scan in the crate can
//! find, so each test below is really asking whether the confirmation step
//! that follows the find is in place — on the read side, on the write side,
//! after a closing quote, and across a round trip.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io::Cursor;

use coseva::config::{
    BlankRecords, EmitOptions, FormatOptions, Headers, ParseOptions, RecordEnding,
};
use coseva::{ByteRecord, Error, IoParser, PushParser, SliceParser, VecEmitter};

/// Read `input`, with no header record.
fn read(input: &[u8], format: FormatOptions) -> Result<Vec<Vec<String>>, Error> {
    let mut parser =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
    collect(&mut parser)
}

fn collect(parser: &mut SliceParser<'_>) -> Result<Vec<Vec<String>>, Error> {
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        records.push(
            line.record()?
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect(),
        );
    }
    Ok(records)
}

fn read_push(input: &[u8], format: FormatOptions, width: usize) -> Result<Vec<Vec<String>>, Error> {
    let mut parser = PushParser::with_options(format, ParseOptions::new().headers(Headers::None))?;
    let mut records = Vec::new();
    let mut record = ByteRecord::new();
    for bytes in input.chunks(width) {
        let mut fed = 0;
        while fed < bytes.len() {
            let mut chunk = parser.chunk(&bytes[fed..]);
            while let Some(mut line) = chunk.next_line()? {
                line.read_byte_record_into(&mut record)?;
                records.push(
                    record
                        .iter()
                        .map(|field| String::from_utf8_lossy(field).into_owned())
                        .collect(),
                );
            }
            fed += chunk.done();
        }
    }
    parser.finish();
    let mut chunk = parser.chunk(b"");
    while let Some(mut line) = chunk.next_line()? {
        line.read_byte_record_into(&mut record)?;
        records.push(
            record
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect(),
        );
    }
    Ok(records)
}

/// Write `records`, with no header record.
fn write(records: &[&[&str]], format: FormatOptions) -> Result<Vec<u8>, Error> {
    let mut emitter =
        VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))?;
    for record in records {
        emitter.emit_record(record.iter().map(|field| field.as_bytes()))?;
    }
    Ok(emitter.into_inner())
}

const PIPES: FormatOptions = FormatOptions::CSV.delimiter_sequence(b"||");

#[test]
fn a_lone_lead_byte_is_data_rather_than_a_delimiter() -> Result<(), Box<dyn StdError>> {
    assert_eq!(
        read(b"a|b||c|||d\n", PIPES)?,
        vec![vec!["a|b".to_owned(), "c".to_owned(), "|d".to_owned()]]
    );
    Ok(())
}

#[test]
fn a_four_byte_delimiter_splits_fields() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.delimiter_sequence(b"<=>|");
    assert_eq!(
        read(b"a<=><=>|b\n", format)?,
        vec![vec!["a<=>".to_owned(), "b".to_owned()]]
    );
    Ok(())
}

#[test]
fn a_multi_byte_terminator_ends_records() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.record_ending_sequence(b"@@");
    assert_eq!(
        read(b"a,b@@c@d,e@@", format)?,
        vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["c@d".to_owned(), "e".to_owned()],
        ]
    );
    Ok(())
}

#[test]
fn ignored_records_require_the_full_multi_byte_terminator() -> Result<(), Box<dyn StdError>> {
    let input = b"@a@@#ignored@inside@@@@b@@#tail@";
    let format = FormatOptions::CSV
        .record_ending_sequence(b"@@")
        .comment(Some(b'#'))
        .blank_records(BlankRecords::Skip);
    let expected = vec![vec!["@a".to_owned()], vec!["b".to_owned()]];

    assert_eq!(read(input, format)?, expected);
    for capacity in [1, 2, 3, 7] {
        let mut parser = IoParser::with_options(
            Cursor::new(input),
            format,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity),
        )?;
        let mut actual: Vec<Vec<String>> = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            actual.push(
                line.record()?
                    .iter()
                    .map(|field| String::from_utf8_lossy(field).into_owned())
                    .collect(),
            );
        }
        assert_eq!(actual, expected, "I/O buffer capacity {capacity}");
    }
    for width in [1, 2, 3, 7] {
        assert_eq!(
            read_push(input, format, width)?,
            expected,
            "push width {width}"
        );
    }
    Ok(())
}

#[test]
fn both_separators_may_be_multi_byte() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter_sequence(b"||")
        .record_ending_sequence(b"@@");
    assert_eq!(
        read(b"a|b||c@@d||e@@", format)?,
        vec![
            vec!["a|b".to_owned(), "c".to_owned()],
            vec!["d".to_owned(), "e".to_owned()],
        ]
    );
    Ok(())
}

#[test]
fn a_quoted_field_may_close_onto_a_multi_byte_separator() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter_sequence(b"||")
        .record_ending_sequence(b"@@");
    assert_eq!(
        read(b"\"a||b\"||\"c@@d\"@@", format)?,
        vec![vec!["a||b".to_owned(), "c@@d".to_owned()]]
    );
    Ok(())
}

#[test]
fn a_partial_separator_after_a_closing_quote_is_rejected() {
    let error = read(b"\"a\"|b||c\n", PIPES).expect_err("a lone lead byte cannot follow a quote");
    assert!(error.to_string().contains("quote"), "{error}");
}

#[test]
fn a_final_record_may_be_unterminated() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.record_ending_sequence(b"@@");
    assert_eq!(
        read(b"a,b@@c,d", format)?,
        vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["c".to_owned(), "d".to_owned()],
        ]
    );
    Ok(())
}

#[test]
fn a_lead_byte_at_the_very_end_stays_in_the_field() -> Result<(), Box<dyn StdError>> {
    assert_eq!(
        read(b"a||b|", PIPES)?,
        vec![vec!["a".to_owned(), "b|".to_owned()]]
    );
    Ok(())
}

#[test]
fn the_emitter_writes_the_whole_sequence() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter_sequence(b"||")
        .record_ending_sequence(b"@@");
    assert_eq!(write(&[&["a", "b"], &["c", "d"]], format)?, b"a||b@@c||d@@");
    Ok(())
}

/// A field holding a lone lead byte is quoted, because appending the delimiter
/// after it would otherwise fuse the two into a separator.
#[test]
fn a_field_holding_a_lead_byte_is_quoted() -> Result<(), Box<dyn StdError>> {
    assert_eq!(write(&[&["a|", "b"]], PIPES)?, b"\"a|\"||b\n");
    Ok(())
}

#[test]
fn what_the_emitter_writes_is_what_the_parser_reads() -> Result<(), Box<dyn StdError>> {
    let records: &[&[&str]] = &[
        &["plain", "row"],
        &["a|b", "c||d"],
        &["|", "||"],
        &["", "trailing|"],
        &["with\nnewline", "with\"quote"],
    ];
    let format = FormatOptions::CSV.delimiter_sequence(b"||");
    let written = write(records, format)?;

    let expected: Vec<Vec<String>> = records
        .iter()
        .map(|record| record.iter().map(|f| (*f).to_owned()).collect())
        .collect();
    assert_eq!(read(&written, format)?, expected);
    Ok(())
}

/// The streaming front end refills its window mid-document, so a separator can
/// straddle a refill; a one-byte buffer forces that at every position.
#[test]
fn the_streaming_parser_agrees_with_the_slice_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"alpha|beta||gamma@@delta||epsilon|@@zeta||eta";
    let format = FormatOptions::CSV
        .delimiter_sequence(b"||")
        .record_ending_sequence(b"@@");
    let options = ParseOptions::new().headers(Headers::None);
    let expected = read(input, format)?;

    for capacity in [1_usize, 2, 3, 7, 64] {
        let mut parser = IoParser::with_options(
            Cursor::new(input.to_vec()),
            format,
            options.clone().buffer_capacity(capacity),
        )?;
        let mut records = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            records.push(
                line.record()?
                    .iter()
                    .map(|field| String::from_utf8_lossy(field).into_owned())
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(records, expected, "buffer capacity {capacity}");
    }
    Ok(())
}

#[test]
fn a_sequence_that_is_too_long_or_empty_is_rejected() {
    for bad in [&b""[..], &b"abcde"[..]] {
        let format = FormatOptions::CSV.delimiter_sequence(bad);
        let error = SliceParser::with_options(b"a\n", format, ParseOptions::new())
            .expect_err("an unusable delimiter sequence is rejected");
        assert!(error.to_string().contains("1 to 4 bytes"), "{error}");
    }
}

#[test]
fn a_sequence_holding_the_quote_byte_is_rejected() {
    let format = FormatOptions::CSV.delimiter_sequence(b"|\"");
    let error = SliceParser::with_options(b"a\n", format, ParseOptions::new())
        .expect_err("a quote inside a separator is rejected");
    assert!(error.to_string().contains("quote or escape"), "{error}");
}

#[test]
fn a_multi_byte_ending_must_be_spelled_as_a_sequence() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .record_ending_sequence(b"@@")
        .record_ending(RecordEnding::CrLf);
    // Setting the ending by variant clears the tail, so this is single-byte
    // again rather than an invalid mixture.
    SliceParser::with_options(b"a\r\n", format, ParseOptions::new())?;
    Ok(())
}

#[test]
fn setting_a_single_byte_delimiter_clears_a_previous_sequence() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV.delimiter_sequence(b"||").delimiter(b';');
    assert_eq!(
        read(b"a|b;c\n", format)?,
        vec![vec!["a|b".to_owned(), "c".to_owned()]]
    );
    Ok(())
}
